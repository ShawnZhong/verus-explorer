# Verus WASM Web Viewer — Project Plan

A browser-based, zero-server interactive viewer for Verus verification and its internal representations (VIR, AIR, SMT queries). Users paste Verus code, see verification results, and can inspect every stage of the pipeline.

---

## Part 1 — What I know about Verus (for this port)

### 1.1 Overview

Verus verifies Rust programs against a specification written inline with macros. The pipeline from Rust source to SMT query is clearly staged:

```
Rust source (with verus! { ... })
   │
   ▼  builtin_macros (proc macro, pure syn — no rustc internals)
Rust source (standard Rust + Verus attributes)
   │
   ▼  rustc (parsing + name resolution + type checking)
rustc HIR + TypeckResults
   │
   ▼  rust_verify::rust_to_vir  (~10K LOC of pattern-matching)
VIR-AST  (vir::ast::Krate)
   │
   ▼  vir::ast_simplify, modes, well_formed
VIR-AST (validated)
   │
   ▼  vir::ast_to_sst
VIR-SST  (pure, no-shadowing form)
   │
   ▼  vir::sst_to_air
AIR  (SMT-level IR)
   │
   ▼  air::smt_process → Z3
Verification result
```

### 1.2 Workspace layout

The Verus repo is a Cargo workspace at `/users/szhong/verus/source/`. Key crates:

| Crate | Purpose | Rustc deps? | Portable to WASM? |
|---|---|---|---|
| `builtin_macros/` | The `verus!` proc macro. Transforms Verus syntax to standard Rust + attributes. | No (uses `syn`) | Yes |
| `vir/` | VIR-AST + VIR-SST + transformations. The "middle end." | No (explicitly) | Yes |
| `air/` | AIR IR + SMT solver communication. | No (explicitly) | Yes, after swapping subprocess for z3-wasm |
| `rust_verify/` | rustc integration. HIR → VIR conversion. | Yes, heavy | **No** |
| `rustc_mir_build/` (forked) | Verus's fork of rustc's MIR builder, with ghost erasure. | Yes, very heavy | **No** — but not needed in `--no-lifetime` mode |
| `vstd/` | Verified standard library. Built & verified as part of toolchain. | N/A (it's input) | Bundled as source files |
| `verus/` | CLI wrapper binary. | Thin | Not needed |

### 1.3 The `--no-lifetime` mode — the key enabler

Verus has a flag `--no-lifetime` (`config.rs:311`) that skips all MIR-related work. When set:

- `verifier.rs:2666-2668` — stubs erasure context to empty
- `verifier.rs:2747-2749` — skips `erase::setup_verus_aware_ids`
- `verifier.rs:2985-2992` — skips `setup_verus_ctxt_for_thir_erasure`
- `verifier.rs:3163-3174` — overrides rustc queries: `mir_built`, `mir_borrowck`, `check_liveness`, `check_mod_deathness` all become no-ops
- `verifier.rs:3306-3307` — skips `trait_check::check_trait_conflicts`
- `verifier.rs:3359-3361` — skips `run_lifetime_checks_on_verus_aware_items`

**Implication:** the entire forked `rustc_mir_build_verus` is dead code in this mode. Ghost erasure doesn't run. Borrow checking doesn't run. But `rustc_hir_analysis::check_crate(tcx)` still runs (`verifier.rs:2753`), and `rust_to_vir::crate_to_vir` still runs (`verifier.rs:2778`). **Verification is unaffected.**

**Soundness caveat:** proofs that would fail the lifetime check can pass in `--no-lifetime` mode. For a production verifier this matters; for an educational viewer with honest UI messaging, it's acceptable.

### 1.4 Rustc dependency surface (what still needs a replacement)

After dropping the MIR/borrow-check half, the remaining rustc surface is:

1. **Parsing** — trivial, can replace with `syn`
2. **Name resolution** — needs cross-module, handles `use` statements, glob imports
3. **Type inference / type checking** — full Hindley-Milner-with-extensions
4. **Trait resolution** — concentrated in 4 files:
   - `resolve_traits.rs` (uses `codegen_select_candidate`)
   - `rust_to_vir_base.rs` (projection normalization)
   - `rust_to_vir_adts.rs` (trait bounds in ADTs)
   - `fn_call_to_vir.rs` (builtin trait dispatch)
5. **HIR structure** — `rustc_hir::ExprKind` (35 variants handled), `TyKind` (34 variants handled)
6. **Type adjustments** — coercions, auto-deref, unsizing (via `TypeckResults::expr_adjustments`)

### 1.5 Z3 dependency

Verus spawns Z3 as a subprocess via stdin/stdout pipes, with dedicated reader/writer threads (`air/src/smt_process.rs`). The `SmtProcess` struct has a clean API (`send_commands`, `wait`) that abstracts the transport. Replacing this with the z3-solver npm package (Emscripten build of Z3) is a well-scoped refactor.

### 1.6 Why not port the upstream Verus project to rust-analyzer

Rustc integration is load-bearing for Verus's soundness story. The main project should stay on rustc. This plan describes a **downstream, parallel tool** — a web viewer that shares `vir/` + `air/` with upstream Verus but has its own frontend.

---

## Part 2 — Project scope

### 2.1 Goal

A web page where a user can:

1. Paste or edit Verus code
2. See verification results (pass/fail, per-function)
3. Inspect intermediate representations side-by-side:
   - Original Rust source (syntax-highlighted)
   - Expanded `verus!` macro output
   - VIR-AST (pretty-printed)
   - VIR-SST (pretty-printed)
   - AIR
   - SMT query sent to Z3
   - Z3's response
4. Click a VIR/AIR node → highlight corresponding source spans
5. Tweak code, see which stage changes

### 2.2 Non-goals (at least for v1)

- Compiling code to an executable (drop MIR/borrow-check entirely)
- Soundness-equivalent behavior to native Verus (be honest about `--no-lifetime` in UI)
- Full Rust language coverage on day one (ship a subset)
- Server-side anything (must work as a static site)
- Replacing the native CLI for production use

### 2.3 Target users

- Students learning formal verification
- Researchers/engineers evaluating Verus
- Verus developers debugging the pipeline
- Anyone writing a blog post or paper who wants an interactive figure

### 2.4 Soundness UI contract

Every verification result ships with a disclaimer when run in `--no-lifetime` mode: *"This viewer skips Rust's borrow checker and lifetime analysis for proofs. Some proofs may verify here that would be rejected by the full Verus CLI. For production use, run `verus` natively."*

---

## Part 3 — Architecture

### 3.1 Component diagram

```
┌──────────────── Browser (static bundle) ─────────────────────┐
│                                                              │
│  ┌─────────────── UI Layer ─────────────────────────────┐    │
│  │  Monaco editor │ IR panels │ SMT log │ result view   │    │
│  └───────────────────┬──────────────────────────────────┘    │
│                      │                                       │
│  ┌───────────────────▼──────────────── Verus WASM ──────┐    │
│  │                                                      │    │
│  │  ┌─ verus!-expand (builtin_macros, WASM) ──────┐    │    │
│  │  │  pre-expand verus! syntax to std Rust        │    │    │
│  │  └──────────────────┬───────────────────────────┘    │    │
│  │                     │                                │    │
│  │  ┌──────────────────▼──── Frontend ──────────────┐  │    │
│  │  │  parse + name-res + type-check + HIR          │  │    │
│  │  │  (Milestone 1: hand-rolled syn-based;          │  │    │
│  │  │   Milestone 2+: rust-analyzer as library)      │  │    │
│  │  └──────────────────┬─────────────────────────────┘  │    │
│  │                     │  our HIR                       │    │
│  │  ┌──────────────────▼─── ra_to_vir (new) ─────────┐  │    │
│  │  │  our HIR → VIR-AST                             │  │    │
│  │  │  (analogous to rust_to_vir, different source)  │  │    │
│  │  └──────────────────┬─────────────────────────────┘  │    │
│  │                     │ VIR-AST                        │    │
│  │  ┌──────────────────▼──── vir/ (unchanged) ──────┐   │    │
│  │  │  simplify → SST → AIR                          │   │    │
│  │  └──────────────────┬─────────────────────────────┘   │    │
│  │                     │ AIR                             │    │
│  │  ┌──────────────────▼──── air/ (patched) ────────┐   │    │
│  │  │  SmtProcess replaced with z3-wasm adapter      │   │    │
│  │  └──────────────────┬─────────────────────────────┘   │    │
│  │                     ▼                                 │    │
│  │              z3-solver npm (WASM)                     │    │
│  └───────────────────────────────────────────────────────┘    │
│                                                              │
│  bundled assets: libcore/libstd/libvstd sources (~5MB)      │
└──────────────────────────────────────────────────────────────┘
```

### 3.2 Data flow

Source text → pre-expand `verus!` → parsed AST → resolved+typed HIR → VIR-AST → (simplify) → VIR-SST → AIR → SMT script → Z3 → result. Every intermediate value is surfaced to the UI.

### 3.3 Deployment

Static site. `index.html` + JS + one `.wasm` file (Verus pipeline) + one `.wasm` file (Z3) + bundled stdlib sources. No backend. Hostable on GitHub Pages / any CDN.

---

## Part 4 — Technology choices

### 4.1 Frontend replacement for rustc

Two-phase approach:

- **Phase A (Milestone 1):** Hand-written frontend using `syn` for parsing and a bespoke type checker for a restricted language subset. Target: the kinds of examples that appear in the Verus tutorial's first half. ~2K LOC of new Rust code. Pros: small, understandable, fast to build. Cons: limited language coverage.

- **Phase B (Milestone 2+):** Swap in rust-analyzer's `hir` + `hir-ty` crates as a library. Gets full Rust support for free. Pros: handles generics, traits, stdlib types. Cons: large binary, API is nominally unstable, some churn.

The Phase A work is *not wasted* when moving to Phase B — the `our HIR → VIR` translator (`ra_to_vir`) can stay structurally similar; only the "source HIR" shape changes.

### 4.2 SMT solver

**z3-solver npm package** ([npmjs.com/package/z3-solver](https://www.npmjs.com/package/z3-solver)). Official Z3 WASM build via Emscripten. Requires `SharedArrayBuffer` for threads, which needs `Cross-Origin-Opener-Policy: same-origin` and `Cross-Origin-Embedder-Policy: require-corp` headers — straightforward on most hosts.

Alternative: cpitclaudel/z3.wasm, bramvdbogaerde/z3-wasm. Back-pocket options if the official one has integration issues.

### 4.3 UI framework

Low-priority decision, pick whatever. Reasonable options:
- Vanilla TS + Monaco editor for the code pane
- React + Monaco + Tailwind
- Yew/Leptos (Rust UI, everything in one language) — appealing but adds build complexity

Recommend starting with vanilla TS or React for velocity; the UI is not the hard part of this project.

### 4.4 Build and tooling

- `wasm-pack` or `wasm-bindgen` for Rust→WASM interop
- `esbuild` or `vite` for JS bundling
- `wasm-opt` (binaryen) for size reduction on release builds
- Pin exact rust-analyzer git revision in Cargo.toml (when Phase B)

### 4.5 Stdlib / vstd bundling

Rust-analyzer needs actual *source* for `core`, `alloc`, `std`, plus Verus's `vstd`. Bundle as an embedded virtual file system — probably via `include_dir!` macro or a serialized-to-binary VFS. Size estimate: ~5-10 MB uncompressed, gzips well.

---

## Part 5 — Staged plan

### Milestone 0 — De-risk spike (1 week)

**Goal:** Prove the back half works in WASM.

1. Carve out `vir/` + `air/` + dependencies into a WASM crate with `wasm-bindgen` exports
2. Build a tiny hand-written VIR-AST for `spec fn add(x: int, y: int) -> int { x + y }` with a trivial `ensures` clause
3. Hook up z3-solver npm
4. Patch `air::smt_process::SmtProcess` to call z3-wasm instead of spawning a subprocess
5. Run verification end-to-end in a Node.js test, then in a browser

**Deliverable:** Browser console shows `verified: true` for a hand-constructed VIR program. No UI yet.

**Why first:** This validates (a) `vir/` and `air/` compile cleanly to WASM (they should — they're pure Rust, but untested on wasm32), (b) z3-wasm can accept the exact SMT dialect Verus produces, (c) binary size is tractable. If any of these fail, the whole project is blocked — better to find out in week 1.

### Milestone 1 — Tier 1 MVP: calculator-level Verus (6-8 weeks)

**Goal:** Working web app that verifies tutorial-chapter-1 examples.

**Language subset supported:**
- `spec fn` and `proof fn` declarations
- `requires` / `ensures` (no `decreases` yet)
- Int/bool literals, variables, `let`
- Arithmetic + comparison + logical binops
- Unary `!`, `-`
- `if`/`else` expressions
- Block expressions with a tail expression
- `return`
- Function calls (only to functions defined in the same snippet)
- Types: `int`, `nat`, `bool`, `Seq<int>`, tuples
- `assert` and `assume` statements

**What to build:**
1. `verus!`-expand pre-pass — compile `builtin_macros` to WASM, run on source text client-side
2. Hand-rolled frontend:
   - `syn::parse_file` for parsing
   - Simple scope-based name resolver (single-file only for now)
   - Type checker (bidirectional, ~500 LOC for this subset)
   - Produce a simple typed-HIR as our own struct tree
3. `ra_to_vir` v0.1 — convert typed HIR to VIR-AST
4. UI:
   - Monaco editor on left
   - Tabs on right: Expanded Rust, VIR-AST, VIR-SST, AIR, SMT query, Z3 result
   - Re-run verification on 500ms debounce after edit
5. Error reporting (parse errors, type errors, verification failures) with source-range highlighting

**Deliverable:** Static site where users paste small examples and get full pipeline visualization.

**Key tests (copy from Verus test suite):**
- `spec fn add(x: int, y: int) -> int { x + y }` with `ensures result == x + y`
- `proof fn comm(x: int, y: int) ensures x + y == y + x { }`
- Examples from `rust_verify_test/tests/basic.rs` that don't use generics

### Milestone 2 — Tier 2: real Verus tutorial (3-5 months after M1)

**Goal:** Most examples from the Verus tutorial (all chapters) work.

**What to add:**
1. Swap hand-rolled frontend for **rust-analyzer as a library**
   - Depend on `ra_ap_hir`, `ra_ap_hir_def`, `ra_ap_hir_ty`, `ra_ap_base_db` (the `ra_ap_*` crates are rust-analyzer components republished to crates.io)
   - Set up an in-memory VFS with the user's source + bundled stdlib + vstd
   - Driver that runs rust-analyzer's type inference and exposes `hir::Semantics`
2. Rewrite `ra_to_vir` against rust-analyzer's HIR:
   - Walk `hir::Function::body_source_map` for expressions
   - Query `InferenceResult::type_of_expr` for types
   - Handle generics via `hir::TypeParam` + substitution
3. Bundle `vstd` sources in the WASM binary
4. Trait resolution for common cases: method calls, simple trait bounds
5. `decreases` / termination checking
6. Enum + struct support
7. Pattern matching (basic)

**Deliverable:** A working viewer that handles the code examples from https://verus-lang.github.io/verus/guide/.

### Milestone 3 — Tier 3: vstd-complete (6-12 months after M2)

**Goal:** 90%+ of Verus test suite passes (in `--no-lifetime` mode).

**What to add:**
1. Full trait resolution (coherence, default methods, supertraits)
2. Associated type normalization
3. Coercions + adjustments (the `expr_adjustments` story)
4. Closures (to the extent Verus supports them)
5. Const generics (limited)
6. Bitvector-specific features
7. Performance optimization — WASM startup time, incremental re-verification

**Deliverable:** Parity with native Verus in `--no-lifetime` mode on non-pathological examples.

### Milestone 4 — Polish (ongoing)

- Saved example gallery
- Permalink URLs with encoded code
- Multi-file projects (multiple tabs in the editor)
- Proof-state stepping UI (show intermediate assertions)
- Z3 unsat-core display for failed verifications
- Export transcript as GitHub Gist (the one "server-ish" feature that might be worth it)

---

## Part 6 — Risks and open questions

### 6.1 Technical risks

**R1: WASM binary size.** Rust-analyzer alone was several MB gzipped. Adding `vir/` + `air/` + z3-wasm puts us at 20-40 MB total. Mitigations: aggressive `wasm-opt`, code splitting (load z3-wasm lazily), maybe ship less of stdlib.

**R2: `vir/` or `air/` doesn't cleanly compile to WASM.** Currently unknown — they're pure Rust but never tested on wasm32. Risks: panicking on `SystemTime::now`, subtle `std::fs` uses I missed, threading assumptions. Mitigation: Milestone 0 tests this before anything else.

**R3: SharedArrayBuffer requirement for z3-wasm.** Some hosts won't set the required COOP/COEP headers. Fallback: use a single-threaded Z3 WASM build (smaller, slower, but no header requirements).

**R4: rust-analyzer internal API churn.** Pinning a specific rust-analyzer commit works but makes upgrades painful. Mitigation: treat this as accepted cost, upgrade quarterly.

**R5: Proc macro in WASM.** `builtin_macros` needs to run on the user's code. Can we compile `builtin_macros` itself to WASM? It's pure `syn` + `quote` + `proc_macro2` — should work, but `proc_macro2` has some platform-specific bits. Plan B: run `verus!` expansion via a simple syntax-tree rewriter written fresh instead of as a proc macro.

**R6: VIR/AIR serialization for UI display.** Verus has some pretty-printing via `verusdoc` and `--log-all` output, but displaying structured VIR in a UI needs thought. Do we render as S-expressions? Tree view? Probably want to add `serde::Serialize` impls on VIR types (if not already present).

### 6.2 Scope risks

**S1: Feature creep toward "production verifier in browser."** Discipline: the soundness caveat is a feature, not a bug — don't try to close it.

**S2: Users submit code that native Verus accepts but this tool rejects.** Inevitable with a subset. Need clear error messages: "this code uses feature X, not yet supported in the web viewer."

**S3: Upstream Verus changes break the port.** Since we depend on `vir/` and `air/` as libraries, Verus API changes will break us. Options: fork them and cherry-pick changes, or work with upstream to stabilize a release cadence.

### 6.3 Open questions

1. Is `vir::ast::Krate` `serde::Serialize`? The import/export code in `rust_verify/src/import_export.rs` serializes it, so yes in some form — need to verify.
2. Does `air` require any filesystem writes in the happy path (not logging)? Need to audit.
3. Will the Verus maintainers be interested in upstreaming small changes needed for this port (stable `vir` API, etc.)?
4. Licensing: Verus is MIT, compatible with everything. Rust-analyzer is MIT/Apache-2.0. Z3 is MIT. All green.
5. Do we want macro-hygienic handling of `verus!` expansion, or is source-level text-substitution enough?

### 6.4 Unknowns I should investigate before committing to M1

- **Binary size estimate.** Build `vir/` + `air/` as `cdylib` targeting `wasm32-unknown-unknown` and see what `wasm-opt -Oz` produces.
- **`import_export.rs` format.** Is it stable? Versioned? What's the wire format for `Krate`?
- **Does `builtin_macros` compile to WASM?** Try it.
- **What does `--log-all` output look like?** Informs UI design.

---

## Part 7 — Key references

### Verus source (all paths relative to `/users/szhong/verus/source/`)

- `rust_verify/src/verifier.rs:2666+` — `--no-lifetime` branch points
- `rust_verify/src/verifier.rs:3135` — `rustc_driver::Callbacks` impl (not needed in our port)
- `rust_verify/src/rust_to_vir*.rs` — what we're rewriting
- `rust_verify/src/resolve_traits.rs` — the trait-resolution code to port
- `vir/src/ast.rs` — VIR-AST definition (our serialization target)
- `vir/src/ast_to_sst.rs` — VIR-AST → VIR-SST (unchanged in port)
- `vir/src/sst_to_air*.rs` — VIR-SST → AIR (unchanged)
- `air/src/smt_process.rs` — the subprocess code to replace
- `air/src/context.rs` — the AIR-level SMT interface (unchanged)
- `builtin_macros/src/syntax.rs` — the `verus!` macro parser (compile to WASM)
- `rust_verify/src/import_export.rs` — existing `Krate` serialization (useful for our IPC)

### External projects

- [Z3 WASM workflow](https://github.com/Z3Prover/z3/blob/master/.github/workflows/wasm.yml) — official Emscripten build
- [z3-solver npm](https://www.npmjs.com/package/z3-solver) — the package to consume
- [rust-analyzer architecture](https://rust-analyzer.github.io/book/contributing/architecture.html) — understand the `hir` API boundary
- [rust-analyzer PR #20329](https://github.com/rust-lang/rust-analyzer/pull/20329) — merged Aug 2025, next-gen trait solver
- [rust-analyzer-wasm (archived)](https://github.com/rust-analyzer/rust-analyzer-wasm) — proof that RA compiles to WASM; use as reference
- [ra_ap_hir on crates.io](https://crates.io/crates/ra_ap_hir) — rust-analyzer components republished for library consumption

### Relevant issues to watch

- [rust-lang/rust#62202](https://github.com/rust-lang/rust/issues/62202) — self-hosting rustc to WASM (aspirational, not happening)
- [rust-lang/miri#722](https://github.com/rust-lang/miri/issues/722) — miri to WASM (not officially supported)
- [rust-lang/rust-analyzer#20422](https://github.com/rust-lang/rust-analyzer/issues/20422) — post-trait-solver-merge tracking

---

## Part 8 — What I'd do this week if I were starting

1. **Day 1-2: Milestone 0 spike setup.** Create a new Cargo workspace at `~/verus-explorer/`. Add `vir/` and `air/` as path dependencies from `/users/szhong/verus/source/`. Try to build for `wasm32-unknown-unknown`. Fix whatever doesn't compile.
2. **Day 3: Hand-construct a VIR program in Rust code.** Pick the simplest possible example (`spec fn identity(x: int) -> int { x }` with `ensures result == x`). Drive it through `vir::ast_to_sst`, `vir::sst_to_air`, and up to the point where it would normally hit Z3.
3. **Day 4: Integrate z3-solver npm.** Write a JS-side adapter that implements the SMT transport. Hook it up to the AIR layer via a trait that abstracts `SmtProcess`.
4. **Day 5: End-to-end in Node.js.** Run the whole spike as a Node script. If `verified: true` comes out, milestone 0 is done.
5. **Day 6-7: Port the spike to a browser page.** Plain HTML + the WASM module + z3-solver browser build. No UI beyond a button that runs the hardcoded example.

If Day 5 fails, stop and diagnose. If it succeeds, you have proof of concept and can commit to Milestone 1 with confidence.
