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

dev: $(DIST)/index.html $(DIST)/z3.js $(DIST)/z3.wasm
	wasm-pack build --dev --target web --out-dir $(DIST)/pkg

release: $(DIST)/index.html $(DIST)/z3.js $(DIST)/z3.wasm
	wasm-pack build --release --target web --out-dir $(DIST)/pkg

$(DIST)/z3.js $(DIST)/z3.wasm &: scripts/build-z3.sh | $(DIST)
	./scripts/build-z3.sh
	cp $(BUILD)/z3.js $(BUILD)/z3.wasm $(DIST)
$(DIST)/index.html: public/index.html | $(DIST)
	cp $< $@

$(DIST):
	git worktree add --orphan -b gh-pages $(DIST)

serve:
	python3 -m http.server --directory $(DIST) 8000

# Each deploy re-creates gh-pages as a single-commit orphan branch in dist/
# and force-pushes, so there's no history either locally or remotely. wasm-pack
# drops an ignore-everything .gitignore in pkg/ on each build; remove it so
# the built artefacts actually stage.
deploy: release
	cd $(DIST) && \
	rm -f pkg/.gitignore && \
	git checkout --orphan _deploy && \
	git add -A && \
	git commit -m "deploy $$(git -C .. rev-parse --short HEAD)" && \
	git branch -M gh-pages && \
	git push --force origin gh-pages

clean:
	rm -rf target $(BUILD)
