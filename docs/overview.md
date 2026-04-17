# verus-explorer — Overview

A browser-based, zero-server interactive viewer for [Verus](https://verus-lang.github.io/verus/) verification and its internal representations (VIR, AIR, SMT queries). The long-term goal is: *paste Verus code, see the verification verdict, inspect every stage of the pipeline*. The current state is a de-risk spike — a browser page that drives the real `vir` + `air` crates through a self-hosted WASM Z3 and shows the verdict for an AIR-text script and a hand-built VIR Krate.

See [architecture.md](architecture.md) for per-component detail and [roadmap.md](roadmap.md) for what's next.

---

## Data flow (target)

```
Rust source (with verus! { ... })
   │
   ▼  builtin_macros (proc macro, pure syn — no rustc internals)
Rust source (standard Rust + Verus attributes)
   │
   ▼  parse + name-res + type-check  (hand-rolled → rust-analyzer)
our HIR + TypeckResults
   │
   ▼  ra_to_vir (new — analogous to upstream rust_to_vir)
VIR-AST  (vir::ast::Krate)                   ◄── today's POC enters here
   │
   ▼  vir::ast_simplify, modes, well_formed, prune
VIR-AST (validated + pruned)
   │
   ▼  vir::ast_to_sst, vir::poly
VIR-SST  (pure, no-shadowing form)
   │
   ▼  vir::sst_to_air_func
AIR  (SMT-level IR)
   │
   ▼  air::context::Context → air::smt_process (wasm32 shim) → Z3_eval_smtlib2_string
Verification result
```

Everything from `vir::ast::Krate` downward runs end-to-end in the browser today. The gray arrow at the top — *source → Krate* — is the focus of the next milestone.

---

## Current status (one line)

Clicking **Run** at `http://localhost:8000` (after `make serve`) feeds three queries through the real `air::context::Context`:

1. `(check-valid … (= (+ x y) (+ y x)))` — expected `Valid`.
2. `(check-valid … (= x 0))` — expected `Invalid`.
3. A hand-built `proof fn lemma() ensures true {}` Krate driven through `simplify_krate → prune → ast_to_sst_krate → poly → func_*_to_air` — expected `Valid`.

SMT is routed through a single-threaded Z3 WASM we build from source. No server. No `SharedArrayBuffer` / COOP+COEP headers. Static hosting works.

---

## What the spike proves

- **`vir/` and `air/` compile to `wasm32-unknown-unknown`** — after one tiny `#[cfg(target_arch = "wasm32")]` shim in `air/src/smt_process.rs` (our fork, commit `b9c84bed`).
- **Z3-in-browser can handle Verus's SMT dialect** — demonstrated by an AIR-text script and by a Krate driven through the real pipeline.
- **The transport abstraction holds** — replacing `SmtProcess`'s subprocess + reader/writer threads with a synchronous call to `Z3_eval_smtlib2_string` on `globalThis` is about 60 lines of Rust behind a `cfg`.
- **No server required** — the whole thing ships as a static bundle (`index.html` + `pkg/` + `z3.{js,wasm}`).

---

## Repo layout

```
verus-explorer/
├── Cargo.toml               # single crate, vir+air via path deps into third_party/verus
├── rust-toolchain.toml      # pinned toolchain (matches the verus submodule)
├── Makefile                 # `make dev | release | serve | package | clean`
├── setup.sh                 # one-time: submodules + wasm32 target + emsdk activate
├── src/
│   ├── lib.rs               # wasm-bindgen entry: run() → Output{queries, all_expected}
│   └── vir_query.rs         # hand-built Krate + VIR→AIR pipeline driver
├── public/
│   └── index.html           # browser page: Z3 bridge, panic hook, UI
├── dist/                    # build output — deploy this directory
└── third_party/             # git submodules
    ├── verus/               # fork: wasm32 SmtProcess shim in air/src/smt_process.rs
    ├── emsdk/               # pinned 3.1.74, activated by setup.sh
    └── z3/                  # built single-threaded via emcmake + emcc
```

---

## How to run

```bash
./setup.sh        # one-time: git submodules, rustup wasm32 target, emsdk install+activate
make serve        # dev build + python3 http.server on :8000
```

Open `http://localhost:8000`, click **Run**. Expected: three query results, final line *"all queries matched expectations"*.

For an optimized build: `make release`. For a tarball: `make package` (produces `verus-explorer.tar.gz` containing everything in `dist/`).

---

## Soundness caveat

Whenever this tool verifies user code (Milestone 1+), it will run Verus in `--no-lifetime` mode — the borrow checker and lifetime analysis are skipped. Proofs that would be rejected by native Verus can pass here. The UI will ship with an honest disclaimer. For production use, run `verus` natively.

This is an intentional non-goal, not a bug to close — see `roadmap.md § non-goals`.
