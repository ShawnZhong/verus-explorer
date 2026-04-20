// Entry point for the CM6 esbuild bundle → `dist/editor.js` (see the
// Makefile recipe). Each line re-exports one symbol that `public/index.html`
// imports from the bundled `./editor.js`; esbuild resolves the bare
// specifiers against `editor/node_modules/` and emits one minified ESM
// bundle with all of CM6's transitive deps inlined. Add a line here when
// `index.html` wants a new CM6 feature (e.g. `lineNumbers`).
export { EditorView, basicSetup } from "codemirror";
export { keymap, Decoration } from "@codemirror/view";
export { StateField, StateEffect, EditorState, Compartment } from "@codemirror/state";
// StreamLanguage is reached transitively through `codemirror`; export it
// so `index.html` can define a tiny inline s-expression parser for the
// SMT-LIB / AIR output tabs without pulling in a legacy-modes package.
export { StreamLanguage, foldService, foldEffect } from "@codemirror/language";
export { indentWithTab } from "@codemirror/commands";
export { rust } from "@codemirror/lang-rust";
export { oneDark } from "@codemirror/theme-one-dark";
export { linter, setDiagnostics } from "@codemirror/lint";
export { search, searchKeymap, openSearchPanel } from "@codemirror/search";
