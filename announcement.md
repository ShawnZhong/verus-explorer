# [RFC] Verus Explorer: in-browser pipeline inspector for Verus

Hey all — sharing a side-project: the full Verus pipeline running in-browser via wasm — no install, no backend. Pick an example, watch every IR from rustc AST through Z3 update live as you type. Aimed at learners, educators, people debugging or reviewing Verus proofs, and contributors poking at the verifier internals.

- **Try it live:** https://shawnzhong.github.io/verus-explorer/
- **Source:** https://github.com/ShawnZhong/verus-explorer

What's there, how it's built, and the fork deltas are in the [README](https://github.com/ShawnZhong/verus-explorer#readme).

## Looking for feedback

I'd love feedback on IR stages, views, or features (from the CLI or beyond) that'd make it a better teaching aid or debugging tool, plus any bugs or in-browser-vs-native divergence you hit. If there's interest, I'll clean up the fork and upstream it so it doesn't drift. Issues, PRs, or just chat here — all welcome.

— Shawn
