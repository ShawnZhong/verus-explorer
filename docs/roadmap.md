# verus-explorer — Roadmap

Forward-looking plan. For current state, see [overview.md](overview.md) and [architecture.md](architecture.md).

---

## Where we are

The de-risk spike is done. `vir/` + `air/` compile to wasm32; a hand-built `proof fn lemma() ensures true {}` Krate drives through the real VIR→SST→AIR pipeline in the browser; Z3 is self-hosted and single-threaded (no COOP/COEP headers required).

Four risks are retired:
- `vir/` + `air/` on wasm32 — works.
- Z3-in-browser speaking Verus's SMT — works.
- `SmtProcess` having a replaceable transport — works, via a wasm32 `#[cfg]` branch.
- `SharedArrayBuffer` / COOP+COEP constraint — sidestepped by building Z3 single-threaded ourselves.

Everything downstream is engineering, not research.

---

## Project goal (target UX)

A web page where a user can:

1. Paste or edit Verus code in Monaco.
2. See verification results (pass/fail, per-function).
3. Inspect intermediate representations side-by-side:
   - Original Rust source (syntax-highlighted)
   - Expanded `verus!` macro output
   - VIR-AST (pretty-printed)
   - VIR-SST (pretty-printed)
   - AIR
   - SMT query sent to Z3
   - Z3's response
4. Click a VIR/AIR node → highlight corresponding source spans.
5. Tweak code, see which stage changes, re-run on debounce.

---

## Non-goals

- Compiling code to an executable (drop MIR / borrow-check entirely).
- Soundness-equivalent behavior to native Verus — we'll run in `--no-lifetime` mode and ship an honest UI disclaimer.
- Full Rust language coverage on day one.
- Server-side anything — must stay a static site.
- Replacing the native CLI for production use.

### Soundness UI contract

Every verification result ships with: *"This viewer skips Rust's borrow checker and lifetime analysis for proofs. Some proofs may verify here that would be rejected by the full Verus CLI. For production use, run `verus` natively."*

---

## Architecture at target

```
Source text
   ↓ builtin_macros (compiled to WASM; or a bespoke syntax-rewriter)
Standard Rust + Verus attributes
   ↓ frontend (hand-rolled syn-based → rust-analyzer as library)
typed HIR
   ↓ ra_to_vir (new — the structural analogue of upstream rust_to_vir)
vir::ast::Krate  ◄─────────────  the existing pipeline starts here today
   ↓ existing vir+air pipeline (unchanged)
Verification result
```

The grey-area work is everything above `vir::ast::Krate`. The `vir+air` + Z3 + JS bridge pieces described in `architecture.md` don't change.

---

## Milestones

### Milestone 1 — Tier 1 MVP: calculator-level Verus (6-8 weeks)

**Goal:** working web app that verifies tutorial-chapter-1 examples from user-typed source.

**Language subset:**
- `spec fn` and `proof fn` declarations
- `requires` / `ensures` (no `decreases` yet)
- Int / bool literals, variables, `let`
- Arithmetic + comparison + logical binops
- Unary `!`, `-`
- `if` / `else` expressions
- Block expressions with tail expression
- `return`
- Function calls (same snippet only)
- Types: `int`, `nat`, `bool`, `Seq<int>`, tuples
- `assert`, `assume`

**Work items:**
1. **`verus!`-expand pre-pass.** Try compiling `builtin_macros` to wasm32. It's pure `syn` + `quote` + `proc_macro2`; should work modulo `proc_macro2`'s platform bits. Plan B: a syntax-tree rewriter written fresh.
2. **Hand-rolled frontend** (`src/frontend/`):
   - `syn::parse_file` for parsing
   - Scope-based name resolver (single file for now)
   - Bidirectional type checker (~500 LOC for this subset)
   - Typed-HIR as our own struct tree
3. **`ra_to_vir` v0.1.** Walk the typed HIR; produce `vir::ast::Krate`. Same shape as today's `vir_query.rs` `build_lemma_krate`, driven by a source walker instead of hard-coded literals.
4. **UI upgrade** (`public/index.html` → Monaco-based):
   - Editor pane left
   - Tabs right: Expanded Rust / VIR-AST / VIR-SST / AIR / SMT query / Z3 result
   - 500 ms debounce on re-verification
5. **Error reporting** with source-range highlighting (parse, type, verification failures).

**Test corpus:** `third_party/verus/source/rust_verify_test/tests/basic.rs` subset that doesn't use generics. Plus:
- `spec fn add(x: int, y: int) -> int { x + y }` with `ensures result == x + y`
- `proof fn comm(x: int, y: int) ensures x + y == y + x { }`

### Milestone 2 — Tier 2: real Verus tutorial (3-5 months after M1)

**Goal:** most examples from <https://verus-lang.github.io/verus/guide/> work.

1. **Swap hand-rolled frontend for rust-analyzer as a library.** Depend on `ra_ap_hir`, `ra_ap_hir_def`, `ra_ap_hir_ty`, `ra_ap_base_db`. In-memory VFS with user source + bundled stdlib + `vstd`. Driver that runs RA's type inference and exposes `hir::Semantics`.
2. **Rewrite `ra_to_vir`** against RA's HIR: `hir::Function::body_source_map`, `InferenceResult::type_of_expr`, generics via `hir::TypeParam` + substitution.
3. **Bundle `vstd` sources** in the WASM binary (probably via `include_dir!`).
4. **Trait resolution** for common cases: method calls, simple bounds.
5. **`decreases` / termination checking.**
6. **Enum + struct support.**
7. **Basic pattern matching.**

### Milestone 3 — Tier 3: vstd-complete (6-12 months after M2)

**Goal:** 90%+ of Verus test suite passes (in `--no-lifetime` mode).

- Full trait resolution (coherence, default methods, supertraits)
- Associated type normalisation
- Coercions + adjustments (`expr_adjustments`)
- Closures (to the extent Verus supports them)
- Const generics (limited)
- Bitvector-specific features
- Performance — startup time, incremental re-verification

### Milestone 4 — Polish (ongoing)

- Saved example gallery
- Permalink URLs with encoded code
- Multi-file projects (multiple tabs)
- Proof-state stepping UI
- Z3 unsat-core display for failed verifications
- Export transcript as GitHub Gist

---

## Immediate next steps (before committing to M1)

1. **Measure the binary.** Add a `wasm-opt -Oz` pass to `make release` and report the size of `dist/pkg/verus_explorer_bg.wasm` + `dist/z3.wasm`. Informs whether binary size is an M1/M2 blocker.
2. **Second VIR fixture.** Extend `src/vir_query.rs` with `spec fn add(x: int, y: int) -> int { x + y }` + a lemma `ensures add(x, y) == x + y`. Confirms the parameter-passing + spec-mode paths beyond `lemma() ensures true`.
3. **`builtin_macros` → wasm32 spike.** 1-day exercise. Answer R5 below before committing to M1's architecture.
4. **Sketch `src/frontend/` module structure.** Parser → name-res → type-check → typed-HIR → `ra_to_vir` → `vir::ast::Krate`. Each boundary is one struct/trait today.
5. **Pick the UI track.** Vanilla TS + Monaco is lowest friction given the current Makefile setup. Only reach for React / Yew if we find a real reason.

---

## Risks

**R1 — WASM binary size.** Rust-analyzer alone was several MB gzipped. Adding `vir/` + `air/` + Z3 + bundled stdlib puts us in the 20-40 MB range. Mitigations: aggressive `wasm-opt`, code splitting (lazy-load Z3), ship less of stdlib.

**R2 — `vir/` or `air/` on wasm32.** ✅ *Retired by the spike.* One additive `#[cfg(target_arch = "wasm32")]` shim in `air/src/smt_process.rs`.

**R3 — SharedArrayBuffer / COOP+COEP.** ✅ *Retired by the spike.* We build Z3 single-threaded ourselves.

**R4 — rust-analyzer internal API churn.** Pinning a specific commit works but makes upgrades painful. Accept it; upgrade quarterly.

**R5 — Proc macro in WASM.** Can `builtin_macros` compile to wasm32? `syn` + `quote` say yes; `proc_macro2`'s platform bits are the risk. Plan B: syntax-tree rewriter written fresh.

**R6 — VIR / AIR serialisation for UI.** Verus has pretty-printing via `verusdoc` and `--log-all`, but structured tree rendering needs thought. Likely want `serde::Serialize` on the VIR types (verify what's already there).

**R7 — Verus fork drift.** We carry at least one commit on top of upstream (`b9c84bed`: wasm32 `SmtProcess` shim + `Instant::now` stub). Every rebase risks conflicts in `air/src/smt_process.rs`. Mitigation: keep the shim purely additive inside a `cfg` block; consider proposing upstream once stable.

**S1 — Feature creep toward "production verifier in browser."** Discipline: the soundness caveat is a feature, not a bug — don't try to close it.

**S2 — Code native Verus accepts but we reject.** Inevitable with a subset. Need clear errors: *"this code uses feature X, not yet supported in the web viewer."*

**S3 — Upstream Verus changes break the port.** We depend on `vir`/`air` as path deps into our Verus submodule — API changes break us on every submodule bump. Cherry-pick carefully or coordinate with upstream.

---

## Open questions

1. Is `vir::ast::Krate` `serde::Serialize`? `rust_verify/src/import_export.rs` serialises it, so yes in some form — verify the format and whether it's stable enough to treat as a public wire format.
2. Does `air` reach for `std::fs` in the happy path (not logging)? The wasm32 shim bypasses subprocess + threads; confirm nothing else touches the filesystem.
3. Will the Verus maintainers take an upstream PR for the wasm32 `SmtProcess` shim and `Instant::now` stub? Would eliminate R7.
4. Licensing: Verus is MIT, rust-analyzer MIT/Apache-2.0, Z3 MIT. All green.
5. Macro-hygienic `verus!` expansion or source-level text substitution — which does the UI path actually need?

---

## Key references

### Verus source (paths relative to `third_party/verus/source/`)

- `rust_verify/src/verifier.rs:2666+` — `--no-lifetime` branch points
- `rust_verify/src/rust_to_vir*.rs` — what we're rewriting
- `rust_verify/src/resolve_traits.rs` — trait resolution to port
- `vir/src/ast.rs` — VIR-AST definition (our target for `ra_to_vir`)
- `vir/src/ast_to_sst.rs` — VIR-AST → VIR-SST (unchanged in port)
- `vir/src/sst_to_air*.rs` — VIR-SST → AIR (unchanged)
- `air/src/smt_process.rs` — subprocess + threads on native; wasm32 branch calls `Z3_eval_smtlib2_string` on `globalThis` (our fork)
- `air/src/context.rs` — AIR-level SMT interface (unchanged)
- `builtin_macros/src/syntax.rs` — `verus!` macro parser (M1 compile-to-WASM candidate)
- `rust_verify/src/import_export.rs` — existing `Krate` serialisation

### External

- [rust-analyzer architecture](https://rust-analyzer.github.io/book/contributing/architecture.html) — hir API boundary
- [rust-analyzer PR #20329](https://github.com/rust-lang/rust-analyzer/pull/20329) — next-gen trait solver, merged Aug 2025
- [rust-analyzer-wasm (archived)](https://github.com/rust-analyzer/rust-analyzer-wasm) — proof RA compiles to WASM
- [`ra_ap_hir` on crates.io](https://crates.io/crates/ra_ap_hir) — RA components republished as libraries
- [Z3 WASM workflow](https://github.com/Z3Prover/z3/blob/master/.github/workflows/wasm.yml) — reference for our emcmake build
- [z3-solver npm](https://www.npmjs.com/package/z3-solver) — the alternative we decided against (requires COOP+COEP)

### Issues to watch

- [rust-lang/rust#62202](https://github.com/rust-lang/rust/issues/62202) — self-hosting rustc to WASM (aspirational, not happening)
- [rust-lang/rust-analyzer#20422](https://github.com/rust-lang/rust-analyzer/issues/20422) — post-trait-solver-merge tracking
