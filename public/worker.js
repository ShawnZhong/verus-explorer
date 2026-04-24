// Verus + Z3 wasm live here so `parse_source`'s synchronous Z3 calls
// (`Z3_eval_smtlib2_string` via `air/src/smt_process.rs`) no longer block
// the main thread. Main never blocks on a Z3 query; the UI (editor input,
// scroll, button clicks) stays live regardless of how long a solve takes.
//
// Classic worker, not module: lets `importScripts` bring in the Emscripten
// z3 glue (which assigns `initZ3` to the worker globalThis), while dynamic
// `import()` still works for the wasm-bindgen ESM. Module workers can't
// `importScripts`, and the current z3.js isn't built with `EXPORT_ES6=1`,
// so classic is the cheap way to avoid rebuilding z3.
//
// Protocol with index.html (both directions use plain `postMessage`):
//   Main → Worker:
//     { type: 'init', stdMode, verusModule, libs: Array<[name, Uint8Array]> }
//     { type: 'parse', runId, source, expandErrors }
//   Worker → Main:
//     { type: 'ready' }
//     { type: 'init-error', error }
//     { type: 'result', runId, textDiags, jsonDiags, dumps, benches, z3, error }
// `dumps` is Array<{section, contents, folds}> — raw tuples from
// `verus_dump`. Main owns the body+fold-range assembly since the UI code
// already knows how to consume that shape.

importScripts('./z3/z3.js');

// Per-parse accumulator. Rewired to a fresh object at the top of every
// 'parse' message; the callback shims below push into whatever this
// currently points at. Null outside of a parse so stray callbacks
// (shouldn't happen) drop silently instead of polluting the next run.
let result = null;

// Rust → JS bridge. Matches the `#[wasm_bindgen]` externs in
// `verus-explorer/src/lib.rs` — same names, same contracts, just attached
// to this worker's globalThis instead of the main thread's. All are
// fire-and-forget accumulators; none need a return value. The batched
// result message at the end of `parse_source` ships everything back to
// main in one shot.
self.verus_diagnostic = (msg) => { result?.textDiags.push(msg); };
self.verus_diagnostic_json = (msg) => {
  try { result?.jsonDiags.push(JSON.parse(msg)); }
  catch (e) { console.warn('verus_diagnostic_json: parse failed', e, msg); }
};
self.verus_dump = (section, contents, folds) => {
  // Ship raw (section, contents, folds); main does the body concat + fold
  // range math since that's coupled to the CM6 rendering code it owns.
  // `contents` is a Vec<String> from Rust, `folds` is Vec<u8>; both
  // structured-clone cleanly.
  result?.dumps.push({ section, contents, folds });
};
self.verus_bench = (label, ms) => { result?.benches.push([label, ms]); };
self.verus_z3_annotate = (label) => { result?.z3.push(`;; ${label}`); };

let verus = null;

self.onmessage = async (e) => {
  const msg = e.data;

  if (msg.type === 'init') {
    try {
      const { stdMode, verusModule, libs } = msg;

      // `locateFile` steers Emscripten to `./z3/z3.wasm` relative to this
      // worker's URL — z3.js was built with `ENVIRONMENT=web` which
      // normally uses `document.currentScript` for URL resolution, but
      // `document` is undefined in a worker, so the override is required.
      const Z3 = await self.initZ3({
        locateFile: (path) => new URL(`./z3/${path}`, self.location.href).href,
      });

      // Z3 shims: `air/src/smt_process.rs` declares these as wasm-bindgen
      // externs, which the instantiated wasm resolves against the realm
      // it runs in (this worker's globalThis). Same wiring as the previous
      // main-thread version in index.html.
      self.Z3_mk_config = Z3.cwrap('Z3_mk_config', 'number', []);
      self.Z3_mk_context = Z3.cwrap('Z3_mk_context', 'number', ['number']);
      self.Z3_del_config = Z3.cwrap('Z3_del_config', null, ['number']);
      self.Z3_del_context = Z3.cwrap('Z3_del_context', null, ['number']);
      const z3Eval = Z3.cwrap('Z3_eval_smtlib2_string', 'string', ['number', 'string']);
      // Tee replies into the per-parse result buffer; interleaved with the
      // `;; label` banners that `verus_z3_annotate` pushes so the Z3 tab
      // reads as labelled stanzas in order, not a flat reply stream.
      self.Z3_eval_smtlib2_string = (ctx, query) => {
        const reply = z3Eval(ctx, query);
        result?.z3.push(reply);
        return reply;
      };

      // `verusModule` is a `WebAssembly.Module` compiled on main and
      // structured-cloned across; init instantiates it here and runs the
      // `#[wasm_bindgen(start)]` hook so proc-macro registration lands.
      const v = await import('./verus_explorer.js');
      await v.default({ module_or_path: verusModule });
      v.set_std_mode(stdMode);
      for (const [name, bytes] of libs) v.wasm_libs_add_file(name, bytes);
      v.wasm_libs_finalize();
      verus = v;
      self.postMessage({ type: 'ready' });
    } catch (err) {
      console.error('worker init failed:', err);
      self.postMessage({ type: 'init-error', error: String(err?.message ?? err) });
    }
    return;
  }

  if (msg.type === 'parse') {
    const { runId, source, expandErrors } = msg;
    result = { runId, textDiags: [], jsonDiags: [], dumps: [], benches: [], z3: [], error: null };
    try {
      verus.parse_source(source, expandErrors);
    } catch (err) {
      // Two common trap shapes:
      //   - `rustc::abort_if_errors` traps as `unreachable` → wasm
      //     runtime error propagated as a JS exception. Rust already
      //     pushed diagnostics through the callbacks before the trap,
      //     so the main-side post-parse flow has something to render.
      //   - V8 stack overflow (deep recursion in HIR lowering / VIR
      //     interpreter exceeds the worker's smaller thread stack) →
      //     shows up as `RangeError: Maximum call stack size exceeded`.
      //     Main detects this pattern and formats a friendly message.
      // Either way, main respawns this worker before the next parse so
      // the wasm instance's statics/heap reset; no per-trap cleanup
      // needed here.
      result.error = String(err?.message ?? err);
      console.error('parse_source trap:', err);
    }
    const payload = result;
    result = null;
    self.postMessage({ type: 'result', ...payload });
    return;
  }
};
