// Entry point for the CM6 esbuild bundle → `public/editor.js` (see the
// Makefile recipe). Each line re-exports one symbol that `public/index.html`
// imports from the bundled `./editor.js`; esbuild resolves the bare
// specifiers against `node_modules/` and emits one minified ESM bundle
// with all of CM6's transitive deps inlined. Add a line here when
// `index.html` wants a new CM6 feature (e.g. `lineNumbers`).
export { EditorView, basicSetup } from "codemirror";
export { keymap } from "@codemirror/view";
export { indentWithTab } from "@codemirror/commands";
export { rust } from "@codemirror/lang-rust";
export { oneDark } from "@codemirror/theme-one-dark";
