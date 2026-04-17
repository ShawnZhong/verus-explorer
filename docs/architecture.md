# verus-explorer — Architecture

Per-component detail for the pieces actually checked in today. See [overview.md](overview.md) for the high-level picture and [roadmap.md](roadmap.md) for what's missing.

---

## Component map

```
┌──────────────── Browser (static bundle) ─────────────────────────────┐
│                                                                      │
│  public/index.html                                                   │
│  ├─ panic hook  (Rust panics → #out)                                 │
│  ├─ Z3 bridge   (Z3_mk_config / Z3_eval_smtlib2_string / … on        │
│  │               globalThis, backed by ccall)                        │
│  ├─ UI          (Run button → calls run() → renders Query list)      │
│  │                                                                   │
│  ├── dist/pkg/verus_explorer.{js,wasm}                               │
│  │   └─ Rust wasm crate (this repo's src/)                           │
│  │      ├─ lib.rs          ← wasm-bindgen entry `run()`              │
│  │      │   • QUERIES[] — AIR-text fixtures                          │
│  │      │   • run_air_text_query  (sise → air::parser → Context)     │
│  │      │   • run_vir_query       (vir_query → prelude → Context)    │
│  │      │   • execute             (drives Context, classifies verdict)│
│  │      └─ vir_query.rs    ← hand-built Krate + full pipeline driver │
│  │                                                                   │
│  │      depends on (via path deps into third_party/verus):           │
│  │      • vir     — unchanged                                        │
│  │      • air     — one #[cfg(target_arch = "wasm32")] shim          │
│  │                                                                   │
│  └── dist/z3.{js,wasm}                                               │
│      └─ Single-threaded Z3 WASM (emcc + libz3.a)                     │
│         • loaded by Emscripten MODULARIZE glue                       │
│         • exposes Z3_* C API via ccall                               │
└──────────────────────────────────────────────────────────────────────┘
```

The three architectural layers from the inside out: **the Z3 WASM runtime**, **the Rust wasm crate** (incl. the `vir`+`air` submodules), and **the browser page / JS bridge**. A fourth cross-cutting concern — **build + submodules** — sits underneath all three.

---

## 1. Z3 WASM runtime

### Source

`third_party/z3` — pinned upstream Z3 as a git submodule.

### Build

Driven by `Makefile`:

```makefile
$(Z3)/build/libz3.a:                          # step 1: build Z3 as a static lib
    emcmake cmake -S $(Z3) -B $(Z3)/build \
      -DZ3_BUILD_LIBZ3_SHARED=OFF \
      -DZ3_SINGLE_THREADED=ON \
      -DCMAKE_BUILD_TYPE=Release

$(DIST)/z3.wasm: $(Z3)/build/libz3.a           # step 2: link into z3.{js,wasm}
    emcc -x c /dev/null $(Z3)/build/libz3.a \
      -s MODULARIZE=1 -s EXPORT_NAME=initZ3 \
      -s EXPORTED_FUNCTIONS='["_Z3_mk_config","_Z3_mk_context","_Z3_del_config",
                              "_Z3_eval_smtlib2_string","_Z3_del_context"]' \
      -s EXPORTED_RUNTIME_METHODS='["ccall"]' \
      -s FILESYSTEM=0 -s ALLOW_MEMORY_GROWTH=1 …
```

### Why single-threaded

The multithreaded Z3 WASM requires `SharedArrayBuffer`, which requires the page to be served with `Cross-Origin-Opener-Policy: same-origin` and `Cross-Origin-Embedder-Policy: require-corp` headers. Many static hosts (GitHub Pages, plain CDNs) don't set these. Building Z3 ourselves with `-DZ3_SINGLE_THREADED=ON` removes the constraint: the site works from any static host at the cost of being slower per-query.

### Symbol surface

We export only what `air::smt_process` needs:

- `Z3_mk_config` / `Z3_del_config`
- `Z3_mk_context` / `Z3_del_context`
- `Z3_eval_smtlib2_string` — the workhorse: takes an SMT script, returns solver stdout

`ccall` handles string marshalling across the wasm stack, so we don't export `_malloc` / `_free`. `FILESYSTEM=0` drops emscripten's synthetic VFS — Z3 never needs it in this use.

---

## 2. Rust wasm crate (`src/`)

Single crate, `crate-type = ["cdylib", "rlib"]`. Published as `dist/pkg/` via `wasm-pack build --target web`.

### `src/lib.rs` — wasm-bindgen entry

Public surface exposed to JS:

| Rust | Role |
|---|---|
| `#[wasm_bindgen] fn run() -> Output` | Entry point — clicked by the Run button. |
| `Output { all_expected: bool, queries: Vec<Query> }` | Aggregate result. |
| `Query { label, air, verdict, proved }` | One query — what the UI renders. |
| `#[wasm_bindgen(start)] fn init()` | Installs a Rust panic hook that calls the JS `reportPanic(msg)` import. |

Private helpers:

- `QUERIES: &[(&str, &str, bool)]` — two AIR-text fixtures + expected verdict.
- `run_air_text_query(label, script)` — parses the AIR script with `sise` + `air::parser::Parser`, executes via `Context`, returns a `Query`.
- `run_vir_query()` — calls `vir_query::run_vir_pipeline()`, prepends `vir::context::Ctx::prelude(…)`, executes.
- `execute(commands)` — spins up `air::context::Context::new(_, SmtSolver::Z3)`, feeds each command, classifies the first non-`Valid` `CheckValid` outcome. Calls `ctx.finish_query()` after each check.

### `src/vir_query.rs` — hand-built Krate + pipeline driver

Proves the real VIR→AIR pipeline runs in wasm32 — not just AIR-text-in.

Builds a minimal `vir::ast::Krate` for:

```rust
proof fn lemma()
    ensures true
{ }
```

Then drives it through every real stage upstream Verus runs (minus rustc + MIR):

1. `GlobalCtx::new` — initialises the global context with `SmtSolver::Z3`, rlimit, etc.
2. `vir::recursive_types::check_traits` — trait well-formedness.
3. `vir::ast_simplify::simplify_krate` — lowering + simplification.
4. `vir::prune::prune_krate_for_module_or_krate` — cull unused items.
5. `Ctx::new` — per-module context with the pruned Krate.
6. `vir::ast_to_sst_crate::ast_to_sst_krate` — VIR-AST → VIR-SST.
7. `vir::poly::poly_krate_for_module` — polymorphism handling.
8. Emit AIR the same way `verify_bucket` does:
   - `ctx.fuel()` globals
   - for each function: `func_name_to_air` + `func_decl_to_air` + `func_axioms_to_air`
   - plus the `FuncCheckSst` body (for proof fns: `exec_proof_check`) via `func_sst_to_air`

Returns a `VirPipelineResult { commands, arch_word_bits, trace }`. `commands` is handed to `air::context::Context`. `trace` is a human-readable stage log surfaced in the UI.

A native unit test (`#[cfg(test)]`) runs the pipeline outside WASM to keep iteration fast during development.

### The `vir` + `air` dependencies

Path deps into the vendored Verus submodule:

```toml
vir = { path = "third_party/verus/source/vir" }
air = { path = "third_party/verus/source/air" }
```

`vir` compiles cleanly to wasm32 with no changes. `air` needs one fork-carried patch: see below.

---

## 3. `air::smt_process` — the wasm32 shim

Native `air` spawns Z3 as a subprocess and talks to it over stdin/stdout with a reader + writer thread (`third_party/verus/source/air/src/smt_process.rs`). Neither subprocesses nor threads exist on `wasm32-unknown-unknown`, so we carry one upstream-fork patch that adds a `#[cfg(target_arch = "wasm32")]` branch:

```rust
#[cfg(target_arch = "wasm32")]
mod wasm {
    use wasm_bindgen::prelude::*;

    #[wasm_bindgen]
    extern "C" {
        fn Z3_mk_config() -> u32;
        fn Z3_mk_context(cfg: u32) -> u32;
        fn Z3_del_config(cfg: u32);
        fn Z3_eval_smtlib2_string(ctx: u32, script: &str) -> String;
        fn Z3_del_context(ctx: u32);
    }

    // … synchronous `send_commands` implementation that calls
    //     Z3_eval_smtlib2_string and splits the response on newlines.
}
```

Rationale:

- **No trait abstraction needed.** The structural shape of `SmtProcess` (constructor + `send_commands`) is preserved; only the body differs per target. Keeps the diff additive and minimises the risk of upstream rebase conflicts.
- **Synchronous.** `Z3_eval_smtlib2_string` returns a complete result string per call, so no async machinery is required on the Rust side. The JS-side `ccall` blocks the main thread — fine for a Run-button workflow, something to revisit if we want streaming progress.
- **`Instant::now()` stub.** `air`'s timing code uses `std::time::Instant::now()`, which panics on wasm32-unknown-unknown. The same fork commit stubs it to a monotonic counter.

The fork commit is `third_party/verus@b9c84bed`. Every upstream rebase should re-land this patch.

---

## 4. Browser page / JS bridge (`public/index.html`)

One self-contained HTML file. Responsibilities:

### Z3 loading

```html
<script src="./z3.js"></script>          <!-- emscripten MODULARIZE glue -->
<script type="module">
  import init, { run } from './pkg/verus_explorer.js';
  const Z3 = await globalThis.initZ3();  // async-load z3.wasm
</script>
```

### Bridging Z3 onto `globalThis`

After `initZ3()` resolves, plain JS functions are installed on `globalThis`. `wasm-bindgen`'s extern imports resolve against the JS global scope, so this is how the Rust-side `extern "C" fn Z3_*` declarations get bound:

```js
globalThis.Z3_mk_config   = ()        => Z3.ccall('Z3_mk_config',   'number', [], []);
globalThis.Z3_mk_context  = (cfg)     => Z3.ccall('Z3_mk_context',  'number', ['number'], [cfg]);
globalThis.Z3_eval_smtlib2_string = (ctx, script) =>
    Z3.ccall('Z3_eval_smtlib2_string', 'string', ['number','string'], [ctx, script]);
// …
```

### Panic hook

`src/lib.rs` declares `fn reportPanic(msg: &str)` as a wasm-bindgen import. `public/index.html` defines `globalThis.reportPanic` to append the message to `#out` — so Rust panics show up on the page, not just in devtools.

### UI

Today: a single **Run** button and one `<pre>` rendering each `Query`'s label, verdict, and AIR trace. Enough to demo the end-to-end loop. Monaco + tabbed IR panels are Milestone 1 work (see `roadmap.md`).

---

## 5. Build + toolchain + submodules

### Submodules (`.gitmodules`)

- `third_party/verus` — our fork of upstream Verus, pinned.
- `third_party/z3` — upstream Z3, pinned.
- `third_party/emsdk` — pinned at `3.1.74`, activated by `setup.sh`.

Pinning all three keeps the build hermetic.

### `setup.sh` (one-time)

```
git submodule update --init --recursive
rustup target add wasm32-unknown-unknown
third_party/emsdk/emsdk install 3.1.74
third_party/emsdk/emsdk activate 3.1.74
```

Idempotent; safe to re-run.

### `rust-toolchain.toml`

Pins the toolchain to the version required by the vendored Verus submodule. Rustup auto-downloads it; no manual action needed.

### `Makefile`

| Target | Effect |
|---|---|
| `make` / `make dev` | `wasm-pack build --dev` + Z3 build. Fastest feedback loop. |
| `make release` | `wasm-pack build --release` — `opt-level = "s"`, LTO, `codegen-units = 1`. |
| `make serve` | `make dev` + `python3 -m http.server --directory dist 8000`. |
| `make package` | `make release` + `tar -C dist -czf verus-explorer.tar.gz .`. |
| `make clean` | Removes `target/`, `dist/`, and the tarball. Keeps `third_party/`. |

`dist/index.html` is a symlink to `public/index.html`, so HTML edits don't need a rebuild — refresh the browser.

Because `emcmake` and `emcc` require `emsdk_env.sh` to be sourced and Make spawns a fresh shell per recipe line, every Z3-building recipe begins with `source $(EMSDK)/emsdk_env.sh`. `SHELL := /bin/bash` is set because emsdk's env script uses `$BASH_SOURCE`.

### Deployment

`dist/` is a self-contained static bundle. Drop it on GitHub Pages, Netlify, any CDN. No backend. No special headers.

---

## 6. Soundness boundary

The `--no-lifetime` path (see [Verus `config.rs:311`](../third_party/verus/source/rust_verify/src/config.rs) and the branch points at `verifier.rs:2666+`, `:2747+`, `:2985+`, `:3163+`, `:3306+`, `:3359+`) skips:

- `mir_built` / `mir_borrowck` / `check_liveness` / `check_mod_deathness`
- `trait_check::check_trait_conflicts`
- `run_lifetime_checks_on_verus_aware_items`
- Ghost erasure (`erase::setup_verus_aware_ids`, `setup_verus_ctxt_for_thir_erasure`)

But `rustc_hir_analysis::check_crate(tcx)` and `rust_to_vir::crate_to_vir` still run — verification logic is unaffected. Native Verus itself supports this mode. Once we replace the rustc frontend, we're structurally in the same regime: verification proofs are sound modulo borrow / lifetime checks.

This is the intentional soundness ceiling for the web viewer. See `roadmap.md § soundness UI contract`.

---

## File index

| Path | Purpose |
|---|---|
| `src/lib.rs` | wasm-bindgen entry (`run`), `Query`/`Output` types, AIR-text driver |
| `src/vir_query.rs` | hand-built Krate + VIR→AIR pipeline driver |
| `public/index.html` | page shell, Z3-on-globalThis bridge, panic hook |
| `Makefile` | dev / release / serve / package / clean |
| `setup.sh` | submodules + wasm32 target + emsdk install/activate |
| `Cargo.toml` | single crate; vir+air via path deps |
| `rust-toolchain.toml` | pinned toolchain (matches verus submodule) |
| `.gitmodules` | pins for verus / z3 / emsdk |
| `third_party/verus` | fork — wasm32 `SmtProcess` shim + `Instant::now` stub |
| `third_party/z3` | upstream Z3, built single-threaded |
| `third_party/emsdk` | pinned emsdk 3.1.74 |
