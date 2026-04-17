# verus-explorer — Bare-Minimum POC

**Goal:** prove the riskiest integration point works — **Z3 WASM can verify the kinds of SMT queries Verus produces, driven by Verus's own AIR encoder, end-to-end in a browser**. No UI. No source parsing. No rust-analyzer. No Verus frontend. Hand-construct the VIR, drive it through the real pipeline, see "verified" in the console.

If this POC works, everything else in the real project is engineering, not research. If it fails, we need to know within a week.

---

## POC success criterion (one line)

**In a browser tab, a JS console log shows `{ verified: true }` for a hand-constructed VIR program that asserts `forall x: int :: x + 0 == x`, having gone through the real `vir` + `air` crates compiled to wasm32, with Z3 running as z3-solver npm.**

That's it. No UI. No editor. No Rust source. One hardcoded example. One console log.

---

## Why this is the right POC

Three independent things could kill the project. This POC exercises all three:

| Risk | How the POC tests it |
|---|---|
| `vir/` + `air/` won't cleanly compile to `wasm32-unknown-unknown` | We build them for that target. Either they compile or they don't. |
| z3-solver npm can't handle the SMT dialect Verus emits (bitvectors, quantifier triggers, custom sorts, etc.) | We feed it a real AIR-generated query and check for `sat`/`unsat`. |
| The `SmtProcess` abstraction in `air/` isn't actually replaceable with an async WASM-land transport | We replace it. Either the trait boundary holds or we discover hidden coupling. |

Nothing else in the full plan matters if any of these three fail.

---

## Scope boundaries

**In scope:**
- Build `vir/` and `air/` as a `cdylib` WASM target
- Hand-written Rust code that constructs a tiny `vir::ast::Krate` in memory
- Run the full VIR → SST → AIR pipeline
- Wire AIR's SMT interface to z3-solver npm
- Node.js driver first (faster feedback loop), then browser

**Out of scope (defer to Milestone 1):**
- Parsing any Rust source
- The `verus!` macro
- Type checking
- rust-analyzer
- Any UI
- Error reporting
- Multiple examples

---

## The hardcoded example

The simplest non-trivial Verus program:

```rust
// Human-readable form (NOT what we'll parse — we hand-write the VIR)
spec fn add_zero(x: int) -> int { x }
proof fn lemma_add_zero_correct(x: int)
    ensures add_zero(x) == x + 0
{
}
```

We manually construct the `vir::ast::Krate` that corresponds to this. It's ~30-50 lines of `Arc::new(...)` calls. Verbose but mechanical — copy from existing Verus log output or from a unit test in the Verus repo.

---

## Plan (5 days)

### Day 1 — Can `vir/` + `air/` compile to WASM?

1. `mkdir ~/verus-explorer && cd ~/verus-explorer && cargo init --lib`
2. Add path dependencies to `/users/szhong/verus/source/vir` and `/users/szhong/verus/source/air` in `Cargo.toml`
3. Add `[lib] crate-type = ["cdylib"]` and `wasm-bindgen = "0.2"`
4. Install the wasm32 target: `rustup target add wasm32-unknown-unknown`
5. `cargo build --target wasm32-unknown-unknown`
6. **Expect failures.** Fix them one by one:
   - `std::fs` uses in `air/` logging code → gate behind a feature flag or stub out
   - `std::time::SystemTime::now()` → replace with `js_sys::Date::now()` or a stub
   - `std::process::Command` in `air::smt_process` → the whole module needs replacing (Day 3)
   - `std::thread::spawn` in `air::smt_process` → same
7. Also try compiling with `--target wasm32-wasip1` as a fallback — WASI has more std support, just in case `unknown-unknown` is too restrictive.

**Deliverable:** a `.wasm` file that links successfully, even if some symbols are stubbed.

**Risk flag:** if `vir/` pulls in a lot of `std` that doesn't exist on wasm32, we need to know.

### Day 2 — Does Z3-WASM speak Verus's SMT?

Before wiring anything up: sanity-check that z3-solver can actually accept the SMT Verus produces.

1. Run native Verus on the hardcoded example with `--log-all`:
   ```bash
   cd /users/szhong/verus/source
   ./vargo run --release --example small -- --log-all
   ```
   (or use any existing test that matches the example)
2. Find the emitted SMT query in `.verus-log/*.smt2`
3. In a fresh Node project: `npm init -y && npm install z3-solver`
4. Write a tiny driver:
   ```js
   const { init } = require('z3-solver');
   const { Z3 } = await init();
   const cfg = Z3.mk_config();
   const ctx = Z3.mk_context(cfg);
   const script = require('fs').readFileSync('query.smt2', 'utf8');
   // Parse + check
   ```
5. Feed the real Verus-emitted SMT2 into z3-solver. Confirm it returns the same result native Z3 gave.

**Deliverable:** Node script that independently verifies z3-solver npm can handle Verus's SMT. No Verus code involved yet.

**Risk flag:** if z3-solver chokes on specific Verus constructs (bitvector operations, specific quantifier patterns, datatype axioms), we need to know and find workarounds (different Z3 version, preprocessing, etc.).

### Day 3 — Replace `SmtProcess` with a WASM adapter

This is the surgery. In the real project we'll do this properly; for the POC, a hack is fine.

1. Introduce a trait in `air/` (or in our new crate wrapping `air/`):
   ```rust
   pub trait SmtTransport {
       fn send_commands(&mut self, commands: Vec<u8>) -> Vec<String>;
   }
   ```
2. Make `air::context::Context` generic over this trait (or add a second constructor)
3. Implement `SmtTransport` twice:
   - `NativeSmtTransport` — wraps the existing `SmtProcess` (for unit tests)
   - `WasmSmtTransport` — calls out to JS via `wasm_bindgen::JsValue` / `web_sys`
4. On the JS side: a function that takes a string of SMT commands, feeds it to z3-solver, returns the response lines.
5. Smoke-test the WASM adapter with the same SMT file from Day 2.

**Deliverable:** `air/`-compiled-to-WASM can send SMT queries to z3-solver npm and get results back.

**Shortcut option:** instead of modifying `air/` itself, implement the transport entirely in our new crate by reimplementing the ~200 lines of `smt_process.rs` to call out to JS. This avoids patching Verus proper — important for keeping the POC scope tight.

### Day 4 — Hand-construct the VIR, run the pipeline

1. In the new crate, write a function `build_example_krate() -> vir::ast::Krate`:
   - Declare a function `add_zero` with mode `Spec`, parameter `x: int`, returning `int`, body `x`
   - Declare a proof function `lemma_add_zero_correct` with `ensures add_zero(x) == x + 0`
   - Wire up the bare minimum `KrateX` surrounding structure (modules, datatypes=empty, traits=empty, etc.)
2. Copy the sequence of `vir` calls from `verifier.rs` that walk a `Krate` through to AIR:
   - `vir::ast_simplify::simplify_krate`
   - `vir::modes::check_crate`
   - `vir::well_formed::check_crate`
   - `vir::ast_to_sst::ast_to_sst_crate`
   - `vir::sst_to_air_func::...`
3. Take the resulting AIR, drive it through the context from Day 3
4. Expose a single `#[wasm_bindgen]` function: `pub fn run_poc() -> String` — returns JSON with `{ verified: bool, smt_query: String, smt_response: String }`

**Deliverable:** from Node.js, calling `run_poc()` returns the verification result for our hardcoded example.

**Biggest risk:** hand-constructing a VIR `Krate` is awkward. The structure has many required fields. **Mitigation:** before writing from scratch, find an existing unit test or the output of `--log-vir` that already serializes a tiny example, and load from that instead.

### Day 5 — Get it running in a browser

1. Minimal HTML page with one button: "Run POC"
2. On click: load the WASM module, call `run_poc()`, dump to `<pre>`
3. Configure the necessary CORS headers for SharedArrayBuffer (z3-solver needs them)
4. Host via `python -m http.server` (or similar) with COOP/COEP headers
5. Open in Chrome, click button, read the console output

**Deliverable:** the target success criterion — browser showing `verified: true` for the hardcoded example, with the SMT query and response visible.

---

## File layout for the POC

```
~/verus-explorer/
├── Cargo.toml              # workspace with WASM crate + JS glue crate
├── crates/
│   └── poc/                # the single WASM crate
│       ├── Cargo.toml      # depends on vir/, air/ via path
│       └── src/
│           ├── lib.rs      # wasm_bindgen entry: run_poc()
│           ├── krate.rs    # hand-constructed Krate
│           ├── pipeline.rs # VIR → SST → AIR driver (copied from verifier.rs)
│           └── smt.rs      # SmtTransport impl calling JS
├── js/
│   ├── package.json        # z3-solver dep
│   ├── node-driver.js      # Days 2-4 driver
│   └── index.html          # Day 5 browser page
└── README.md               # how to run
```

No workspace sharing with `/users/szhong/verus/` — we depend on it via path deps only, so upstream Verus stays untouched.

---

## Daily kill criteria

At the end of each day, ask: did we make it past the day's deliverable? If **no on two consecutive days**, stop and reassess. The whole point of a 5-day POC is that it either demonstrates viability or reveals a killer problem quickly.

---

## What a successful POC proves

1. **`vir/` and `air/` work on wasm32** — the pure-Rust parts of Verus are genuinely portable
2. **z3-solver npm speaks Verus's SMT** — the biggest external dependency is compatible
3. **`SmtProcess` has a clean enough seam to replace** — the transport abstraction holds
4. **The pipeline runs without rustc** — the verification logic doesn't secretly reach into rustc somewhere we missed

With those four things proven, Milestone 1 of the full plan becomes a focused engineering effort: replace the hand-constructed Krate with one derived from user source code. Everything else stays the same.

---

## What a failed POC teaches

Even a failure has value:

- **If Day 1 fails** (vir/air can't compile): we learn what `std` uses exist. Maybe we need a `wasm32-wasip1` target instead, or a preprocessing pass through `air/` to excise logging code. Potentially a fork of vir/air for WASM use — annoying but scoped.
- **If Day 2 fails** (z3 incompatible): the project isn't dead but the SMT layer needs more thought. Maybe use a different Z3 build, or preprocess queries.
- **If Day 3 fails** (SmtProcess won't abstract cleanly): we need a deeper refactor of `air/` — proposal to upstream, or maintain a patch.
- **If Day 4 fails** (hand-constructing Krate too hard): use `--log-vir` output as input format. Shifts our IPC strategy.
- **If Day 5 fails** (WASM doesn't run in browser despite Node working): usually a `SharedArrayBuffer` / header issue, documented and fixable.

Each failure points at a specific, scoped follow-up — not a project-killing unknown.

---

## After the POC

If green by end of Day 5: commit to Milestone 1 (Tier 1 MVP) from the main plan. Start building out the hand-rolled `syn`-based frontend and UI.

If red: write a one-page postmortem, pick the most failed assumption, decide whether the project is still viable or needs a different architecture (e.g., thin-server hybrid after all).

Either way, you've spent one week — much cheaper than finding out at month three.
