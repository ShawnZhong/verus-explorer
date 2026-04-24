// verus-explorer — frontend. Entry point loaded by index.html as a
// module script. Imports CodeMirror 6 from the esbuild-produced
// ./editor.js bundle, brings up the in-browser rustc/Verus/Z3 wasm
// instances, and wires the UI around them.
//
// Sections in source order — each is titled by the comment banner above
// it; grep the title to jump:
//   - Libs manifest + wasm cold-load (`wasmReady` IIFE)
//   - DOM refs + tiny state
//   - Output pane model (tab groups, section caches)
//   - Error-line decoration in the source editor
//   - Renderers
//   - Rust → JS bridge
//   - `runParse` pipeline driver
//   - URL hash sync
//   - Auto-verify
//   - CM6 source editor
//   - Wiring: example dropdown + Verify button

// CodeMirror 6 bundle, built by `make dev` from `scripts/editor/editor-src.js`
// (the single source of truth for CM6 exports; add a line there to expose
// a new feature, then import it here).
import {
  EditorView, basicSetup, keymap, indentWithTab, rust, oneDark,
  Decoration, StateField, StateEffect, EditorState,
  linter, setDiagnostics,
  search,
  Compartment, StreamLanguage, foldService, foldEffect,
  ViewPlugin, RangeSetBuilder,
} from './editor.js';

// libs bundle: rustc-in-wasm resolves `extern crate core/alloc/vstd`
// against rmetas laid out at `/virtual/lib/rustlib/wasm32-unknown-unknown/lib`.
// The Makefile stages each file gzipped as `${name}.gz` under
// `./libs/` (with mode-specific subdirs `./libs/std/` and `./libs/nostd/`);
// we fetch each `.gz`, pipe through the native `DecompressionStream`
// (gzip is supported in all evergreen browsers; brotli isn't yet),
// then re-install into every fresh wasm instance via
// `wasm_libs_add_file` + `wasm_libs_finalize`. The names are the
// uncompressed filenames rustc's crate locator expects in memory,
// *not* what's on the wire.
//
// Two bundle modes picked by URL param `?std=1` (opt-in, default
// is nostd):
//   nostd (default) — no libstd; vstd built with only
//                     `feature="alloc"`, user code is `#![no_std]`.
//                     ~13 MB gzipped, ~20% faster warm verify.
//   std             — libstd + its wasm32 dep chain under
//                     ./libs/std/, vstd built with `feature="std"`.
//                     Unlocks PPtr::new / HashMap / println!. ~16
//                     MB gzipped, ~30-50 ms slower per warm verify.
//                     Opt in via `?std=1` or the toolbar checkbox.
// Dropped from both lists despite being declared libstd deps:
// libpanic_{abort,unwind} — rustc doesn't fetch panic runtimes
// under `--emit=metadata` since no linking happens (measured).
// Backtrace crates (gimli, object, addr2line, miniz_oxide, adler2,
// memchr) don't appear because we disabled `rust.backtrace` in
// `third_party/rust/bootstrap.toml` so libstd itself stops
// declaring them. libtest, libproc_macro, libgetopts, libsysroot,
// and librustc_std_workspace_std are x.py by-products libstd
// doesn't depend on.
const stdMode = new URLSearchParams(location.search).get('std') === '1';
const LIBS_SHARED = [
  'liballoc.rmeta',
  'libcompiler_builtins.rmeta',
  'libcore.rmeta',
  'libverus_builtin.rmeta',
  'libverus_builtin_macros.rmeta',
  'libverus_state_machines_macros.rmeta',
];
// Mode-specific fetches. Paths are relative to `./libs/`. The
// `libvstd.rmeta` / `vstd.vir` entries under std/ and nostd/ have
// the same *logical* names on the rustc side — we register them
// with those names via `wasm_libs_add_file` — but the bytes come
// from the mode's subdir.
const LIBS_STD_ONLY = [
  'std/libstd.rmeta',
  'std/libcfg_if.rmeta',
  'std/libdlmalloc.rmeta',
  'std/libhashbrown.rmeta',
  'std/liblibc.rmeta',
  'std/librustc_demangle.rmeta',
  'std/librustc_std_workspace_alloc.rmeta',
  'std/librustc_std_workspace_core.rmeta',
  'std/libstd_detect.rmeta',
  'std/libunwind.rmeta',
  'std/libvstd.rmeta',
  'std/vstd.vir',
];
const LIBS_NOSTD_ONLY = [
  'nostd/libvstd.rmeta',
  'nostd/vstd.vir',
];
const LIBS = [...LIBS_SHARED, ...(stdMode ? LIBS_STD_ONLY : LIBS_NOSTD_ONLY)];

// Kick off wasm loading in the background (Z3 init, Verus wasm compile,
// libs fetch+decompress — all independent, so fire as one Promise.all;
// critical path = max, not sum). We deliberately do NOT await here: the
// editor, tab strip, and example dropdown are cheap to mount and should
// appear immediately so the user can start reading / editing during the
// multi-MB cold-load. `verus` stays `null` until the chain resolves;
// the Verify button is disabled and `runParse` is a no-op until then.
// The bottom of this script `await`s `wasmReady` to flip the button
// live and kick off the first verify.
let verus = null;
// z3 transcript buffer — hoisted out of the IIFE so `runParse` (which
// clears it at the top of each run and reads it at the bottom) sees the
// same binding the Z3 eval wrapper pushes into. Reassigned (not just
// cleared) by `runParse` to `[]` each run; the wrapper's closure
// captures the *variable*, not the initial array, so the push keeps
// targeting the live buffer.
let z3ResponseBuffer = [];
// Progress counter for cold-load. Each `libs/*.gz` fetch bumps this
// when it resolves and updates the Verify button label, so the user
// sees "Loading 3/18 libs…" instead of a static "Loading verifier…"
// while the multi-MB download streams in. Safe to reference
// `parseButtonLabel` from the async callback even though the DOM
// ref is grabbed later in the script — the callback fires after
// the first microtask, by which point all const declarations have
// executed.
let wasmLibsLoaded = 0;
const wasmReady = (async () => {
  const [Z3, verusModule, wasmLibs] = await Promise.all([
    globalThis.initZ3(),
    WebAssembly.compileStreaming(fetch('./verus_explorer_bg.wasm')),
    Promise.all(LIBS.map(async path => {
      const res = await fetch(`./libs/${path}.gz`);
      const stream = res.body.pipeThrough(new DecompressionStream('gzip'));
      const buf = await new Response(stream).arrayBuffer();
      wasmLibsLoaded++;
      parseButtonLabel.textContent = `Loading ${wasmLibsLoaded}/${LIBS.length} libs…`;
      // Strip the mode subdir prefix ('std/' or 'nostd/') so the
      // in-wasm crate locator sees plain `libvstd.rmeta` etc.
      const name = path.replace(/^(std|nostd)\//, '');
      return [name, new Uint8Array(buf)];
    })),
  ]);

  // Bridge Rust <-> emscripten Z3: air/src/smt_process.rs declares these as
  // wasm-bindgen externs, so each must be a plain JS function on globalThis.
  globalThis.Z3_mk_config = Z3.cwrap('Z3_mk_config', 'number', []);
  globalThis.Z3_mk_context = Z3.cwrap('Z3_mk_context', 'number', ['number']);
  globalThis.Z3_del_config = Z3.cwrap('Z3_del_config', null, ['number']);
  globalThis.Z3_del_context = Z3.cwrap('Z3_del_context', null, ['number']);
  // `Z3_eval_smtlib2_string` returns the solver's textual reply (`sat` /
  // `unsat` / errors / model output) for each batch of SMT commands sent
  // from the AIR driver. The AIR log writers capture *input* SMT-LIB2 but
  // the replies are otherwise opaque; tee them into `z3ResponseBuffer`
  // here so `runParse` can stash the transcript as a 'Z3' section.
  const z3Eval = Z3.cwrap('Z3_eval_smtlib2_string', 'string', ['number', 'string']);
  globalThis.Z3_eval_smtlib2_string = (ctx, query) => {
    const reply = z3Eval(ctx, query);
    z3ResponseBuffer.push(reply);
    return reply;
  };

  // One wasm instance, reused for every `parse_source` call. Each
  // `run_compiler` builds its own Session+CStore; nothing leaks across
  // calls (confirmed by `tests/smoke.rs`). V8 JIT + rustc LazyLock entries
  // survive across calls and amortize, so #2/#3 run at ~2× #1 speed.
  const v = await import('./verus_explorer.js');
  await v.default({ module_or_path: verusModule });
  // Tell the wasm side which vstd bundle is loaded so
  // `build_rustc_config` injects `#![no_std]` in nostd mode (but
  // not in std mode, where user code should see the std prelude).
  v.set_std_mode(stdMode);
  for (const [name, bytes] of wasmLibs) v.wasm_libs_add_file(name, bytes);
  v.wasm_libs_finalize();
  verus = v;
})();

// ------------------------------------------------------------------
// DOM refs + tiny state
// ------------------------------------------------------------------
const parseButton = document.getElementById('parse-run');
const parseButtonLabel = document.getElementById('parse-run-label');
const parseMeta = document.getElementById('parse-meta');
// Drag the splitter above the diagnostics pane to reshape the left
// column. Adjusts `#parse-meta`'s flex-basis; the editor (flex: 1)
// absorbs the rest. Bounded below by 40px so the pane can't vanish
// behind the handle, and above by 90% of the column so the editor
// always has a usable strip.
{
  const resizer = document.getElementById('left-resizer');
  const leftPane = document.getElementById('left-pane');
  let startY, startH;
  const onMove = (e) => {
    const dy = startY - e.clientY;
    const maxH = leftPane.getBoundingClientRect().height * 0.9;
    // Strip the CSS `max-height: 30%` cap once the user drags —
    // otherwise the stylesheet rule beats the inline flex-basis
    // and the pane refuses to grow past 30% of the column.
    parseMeta.style.maxHeight = 'none';
    parseMeta.style.flex = `0 0 ${Math.min(maxH, Math.max(40, startH + dy))}px`;
  };
  const onUp = () => {
    resizer.classList.remove('dragging');
    document.removeEventListener('mousemove', onMove);
    document.removeEventListener('mouseup', onUp);
  };
  resizer.addEventListener('mousedown', (e) => {
    startY = e.clientY;
    startH = parseMeta.getBoundingClientRect().height;
    resizer.classList.add('dragging');
    document.addEventListener('mousemove', onMove);
    document.addEventListener('mouseup', onUp);
    e.preventDefault();
  });
}
// Drag the splitter between the source pane and the output pane to
// reshape the workbench. Same shape as the vertical splitter above:
// rewrite `grid-template-columns` so the left column becomes an
// explicit px width and the right stays `1fr`. The middle column is
// the resizer itself (5px). Clamped so each side keeps a usable
// minimum strip.
{
  const resizer = document.getElementById('col-resizer');
  const workbench = document.getElementById('workbench');
  let startX, startLeftW;
  const onMove = (e) => {
    const totalW = workbench.getBoundingClientRect().width;
    const minW = 120;
    const dx = e.clientX - startX;
    const newLeftW = Math.min(totalW - minW, Math.max(minW, startLeftW + dx));
    workbench.style.gridTemplateColumns = `${newLeftW}px 10px minmax(0, 1fr)`;
  };
  const onUp = () => {
    resizer.classList.remove('dragging');
    document.removeEventListener('mousemove', onMove);
    document.removeEventListener('mouseup', onUp);
  };
  resizer.addEventListener('mousedown', (e) => {
    startX = e.clientX;
    startLeftW = document.getElementById('left-pane').getBoundingClientRect().width;
    resizer.classList.add('dragging');
    document.addEventListener('mousemove', onMove);
    document.addEventListener('mouseup', onUp);
    e.preventDefault();
  });
}
const parseTabsEl = document.getElementById('parse-tabs');
const parseSubtabsEl = document.getElementById('parse-subtabs');
const parseOutputViewEl = document.getElementById('parse-output-view');
const sourceSelect = document.getElementById('source-select');
const autoVerifyCheckbox = document.getElementById('auto-verify');
const expandErrorsCheckbox = document.getElementById('expand-errors');
// std toggle: initial state reflects the URL param (`?std=1` opts
// in; default is nostd). Flipping it rewrites the URL and reloads,
// because the libs bundle is picked up at wasm instance init and
// can't be swapped mid-session without rebuilding the cstore.
const stdModeCheckbox = document.getElementById('std-mode');
stdModeCheckbox.checked = stdMode;
stdModeCheckbox.addEventListener('change', () => {
  const params = new URLSearchParams(location.search);
  if (stdModeCheckbox.checked) params.set('std', '1');
  else params.delete('std');
  const qs = params.toString();
  location.search = qs ? `?${qs}` : '';
});
const downloadBtn = document.getElementById('download-btn');
const shareBtn = document.getElementById('share-btn');

// Example manifest: grouped by topic so the dropdown reads as a mini
// table of contents. Each `items` entry drives one `<option>`; the
// `group` label renders as a native `<optgroup>` header. `file` names
// a source file under `public/examples/` (copied to `dist/examples/`
// by the Makefile). Add an entry here to add a new example — no other
// wiring needed. The first entry also doubles as the cold-load default
// when no URL hash is present.
const EXAMPLE_GROUPS = [
  { group: 'Tutorial', items: [
    { file: 'arith.rs', label: 'Arithmetic' },
    { file: 'requires_ensures.rs', label: 'Requires / ensures' },
    { file: 'collections.rs', label: 'Seq / Set / Map' },
    { file: 'loop.rs', label: 'Loop invariant' },
    { file: 'recursion.rs', label: 'Recursive spec fn' },
    { file: 'struct_invariant.rs', label: 'Struct + ghost binding' },
  ]},
];
const EXAMPLES = EXAMPLE_GROUPS.flatMap(g => g.items);
// Placeholder option shown when the editor holds a custom (hash-loaded
// or externally-pasted) doc that doesn't match any shipped example.
// `hidden` keeps it out of the opened dropdown list; `disabled` blocks
// click selection. Selected programmatically via `sourceSelect.value = ''`.
const customOption = document.createElement('option');
customOption.value = '';
customOption.textContent = '— custom —';
customOption.disabled = true;
customOption.hidden = true;
sourceSelect.appendChild(customOption);
// `optionByFile` lets `parseHash` validate `#source=<file>` hashes
// against the actual manifest without re-scanning the dropdown DOM.
const optionByFile = new Map();
for (const { group, items } of EXAMPLE_GROUPS) {
  const og = document.createElement('optgroup');
  og.label = group;
  for (const { file, label } of items) {
    const opt = document.createElement('option');
    opt.value = file;
    opt.textContent = label;
    og.appendChild(opt);
    optionByFile.set(file, opt);
  }
  sourceSelect.appendChild(og);
}
const prevBtn = document.getElementById('source-prev');
const nextBtn = document.getElementById('source-next');
const resetBtn = document.getElementById('source-reset');
const fetchExample = (file) => fetch(`./examples/${file}`).then(r => r.text());

// ─── Example state ────────────────────────────────────────────────
// `loadedSource` is the currently-loaded shipped example's filename
// (a key of `optionByFile`), or null if the editor holds custom
// content (hash-loaded `#code=…`, pasted legacy hash, or edited-off
// from an example then navigated to custom via some future path).
// `pristineSource` is the shipped source for `loadedSource`, held
// so dirty-detection and Reset don't require a refetch.
// `STORAGE_PREFIX + file` is the per-example localStorage key: the
// stored value is the editor's source the last time it diverged from
// pristine. Removed on Reset; refreshed on every edit.
const STORAGE_PREFIX = 've-source:';
let loadedSource = null;
let pristineSource = null;
// Every caller runs post-mount (updateListener fires only on dispatched
// transactions; the explicit `updateSourceUI()` in the wiring block
// runs after `const view = …`). So `view` is always defined here.
const isDirty = () => loadedSource !== null
  && view.state.doc.toString() !== pristineSource;
// Rewrites the dropdown selection, Reset-button visibility, and
// prev/next enabled state from the current values of `loadedSource`
// + doc contents. Call on every state transition (load / reset /
// edit / external hash-change). The Reset button alone signals the
// dirty state; the dropdown labels stay pristine.
const updateSourceUI = () => {
  sourceSelect.value = loadedSource ?? '';
  resetBtn.hidden = !isDirty();
  const idx = loadedSource === null
    ? -1
    : EXAMPLES.findIndex(e => e.file === loadedSource);
  // When the editor holds custom content (idx === -1), both nav
  // buttons stay enabled — they act as "jump into the example set"
  // (prev → last, next → first) rather than being dead buttons.
  prevBtn.disabled = idx === 0;
  nextBtn.disabled = idx === EXAMPLES.length - 1;
};
// Two-level nav for the output pane: top tabs pick the pipeline stage,
// subtabs pick the variant within that stage. Single-variant groups
// render no subtab row. `id` is the stable internal key (URL hash,
// bench bucket, state maps); `label` is the user-facing tab text —
// spelled out so newcomers don't need to know what HIR/VIR/AIR stand for.
const TAB_GROUPS = [
  { id: 'Rust',  label: 'Rust IR',     variants: ['AST_PRE', 'AST', 'HIR', 'HIR_TYPED'] },
  { id: 'VIR',   label: 'Verus IR',    variants: ['VIR', 'SST_AST', 'SST_POLY'] },
  { id: 'AIR',   label: 'Assert IR',   variants: ['AIR_INITIAL', 'AIR_MIDDLE', 'AIR_FINAL'] },
  { id: 'Z3',    label: 'Z3 Solver',   variants: ['SMT', 'Z3'] },
];
const VARIANT_LABEL = {
  AST_PRE: 'AST', AST: 'Expanded AST', HIR: 'HIR', HIR_TYPED: 'Typed HIR',
  VIR: 'AST', SST_AST: 'SST', SST_POLY: 'Mono',
  AIR_INITIAL: 'Blocks', AIR_MIDDLE: 'SSA', AIR_FINAL: 'Flat',
  SMT: 'Query', Z3: 'Response',
};
// Flat section order + section → group lookup, both derived from
// TAB_GROUPS so adding a new variant is a one-line change.
const SECTION_ORDER = TAB_GROUPS.flatMap(g => g.variants);
const GROUP_OF = new Map(TAB_GROUPS.flatMap(g => g.variants.map(v => [v, g.id])));
// Remembers the last subtab the user clicked in each group, so
// clicking back into (say) `VIR` returns to `VIR-sst` if that's where
// they left it rather than snapping back to the first variant.
const lastVariantInGroup = new Map();

// Rust-declared fold ranges per section — populated by `verus_dump`
// when it receives a block flagged `fold: 1` (e.g. the vstd appendix
// on VIR / SST_AST / SST_POLY). Each entry is `[{from, to}, …]` with
// absolute offsets into the rendered body. `renderOutputView` auto-
// applies them on cold render; `sectionFold` below re-offers them
// to the fold gutter so the user can refold after expanding.
const sectionFolds = new Map();

// `parse_source` always produces every IR; bodies are cached here so
// flipping a tab just swaps the output-view's doc instead of re-parsing.
const sectionCache = new Map();
let verdictCache = null;
// Raw buffers the wasm callbacks push into during `parse_source`.
// `_textDiags` holds `HumanEmitter` output (colorized terminal form);
// `_jsonDiags` holds parsed `JsonEmitter` objects (with `spans[]`
// carrying rustc-exact line/col ranges). `runParse` consolidates them
// into the unified `diagnostics` list below once parsing returns.
const _textDiags = [];
const _jsonDiags = [];
// Unified diagnostic list consumed by the three renderers (`renderMeta`,
// `updateErrorDecorations`, `computeInlineDiagnostics`). Each entry:
//   { rendered: string,                    // text for pane + tooltip
//     severity: 'error'|'warning'|'note',
//     loc: { line, col, endLine?, endCol? } | null }
// When the JSON channel produced entries we use them (rustc-exact
// spans with end positions); otherwise we fall back to the text
// channel with a regex-extracted `line:col` and no end. Built once
// per parse so all three consumers see the same snapshot.
const diagnostics = [];
// Per-stage wall-clock timing, populated by the `verus_bench` callback
// from Rust (one entry per `time(label, ||...)` wrapper in `src/lib.rs`).
// Rendered by `renderMeta` as a grouped breakdown under the verdict.
const benchCache = new Map();
// Collapse the raw Rust-side labels (`rustc_parse`, `verify.queries`,
// `dump.hir_typed`, …) into a handful of user-facing buckets that
// match the tab groups. Unknown labels fall through (ignored) so new
// Rust-side timers don't surprise the user until we classify them.
// `verify` / `verify.module` are wrapper labels (skipped here to
// avoid double-counting their downstream `verify.*` children).
const BENCH_GROUP = {
  rustc_parse: 'Rust',
  'dump.ast_pre': 'Rust', 'dump.ast': 'Rust',
  'dump.hir': 'Rust', 'dump.hir_typed': 'Rust',
  build_vir: 'VIR', 'build_vir.vstd_deserialize': 'VIR',
  'build_vir.build_vir_crate': 'VIR', 'dump.vir': 'VIR',
  'verify.ast_to_sst': 'VIR', 'dump.sst_ast': 'VIR',
  'verify.poly': 'VIR', 'dump.sst_poly': 'VIR',
  'verify.prune': 'AIR', 'verify.ctx_new': 'AIR',
  'verify.air_ctx_new': 'AIR', 'verify.feed_decls': 'AIR',
  'verify.ctx_free': 'AIR', 'verify.reporter_new': 'AIR',
  'verify.queries': 'Z3',
};
// Which pipeline-timing bucket each top-level tab group maps to, so
// the subtab row can surface the bucket matching the current view.
const TAB_GROUP_BENCH = {
  Rust: 'Rust', VIR: 'VIR', AIR: 'AIR', Z3: 'Z3',
};
// Which IR the output view currently shows; preserved across runParse
// calls so the user doesn't lose their tab selection on every edit.
// Null until the first successful parse; `renderTabs` picks a default.
let currentTab = null;
// Per-tab scroll position for the output viewer, so flipping between
// tabs (or re-running verification on the same tab) returns to where
// the user was reading. Kept live by a `scroll` listener on the
// viewer's scrollDOM, below; restored in `renderOutputView`.
const tabScrolls = new Map();

// ------------------------------------------------------------------
// Error-line decoration in the source editor. A StateField holds a
// RangeSet of `Decoration.line({ class: 'cm-error-line' })` positions;
// `setErrorLines` rebuilds it on every parse so stale marks from a
// prior run clear before new ones land. `decoration.map(tr.changes)`
// keeps existing marks following edits until the next parse refreshes.
// ------------------------------------------------------------------
const setErrorLines = StateEffect.define();
const errorLineField = StateField.define({
  create: () => Decoration.none,
  update(deco, tr) {
    deco = deco.map(tr.changes);
    for (const e of tr.effects) {
      if (e.is(setErrorLines)) deco = Decoration.set(e.value, true);
    }
    return deco;
  },
  provide: f => EditorView.decorations.from(f),
});
const errorLineDeco = Decoration.line({ attributes: { class: 'cm-error-line' } });
const warningLineDeco = Decoration.line({ attributes: { class: 'cm-warning-line' } });

// Rustc's `HumanEmitter` prefixes each top-level diagnostic with the
// level name: `error[E0432]:`, `error:`, `warning:`, `note:`, `help:`.
// We key off that first token to pick severity. Anything we don't
// recognize (rare — e.g., a Verus-side message without the prefix)
// falls through as an error so it stays visible in red.
const severity = (msg) => {
  const m = msg.trimStart().match(/^(error|warning|note|help)\b/);
  if (!m) return 'error';
  if (m[1] === 'note' || m[1] === 'help') return 'note';
  return m[1];
};

// ------------------------------------------------------------------
// Renderers
// ------------------------------------------------------------------
// Pull a `line:col` out of a rustc diagnostic so we can make it clickable
// *and* surface it as a source-editor line mark. The `--> file:N:M` form
// is rustc's canonical span marker; fallback to any `N:M` occurrence
// covers formats that don't carry an arrow (e.g., Verus-side errors).
// Only the first match is used.
const extractLoc = (msg) => {
  const m = msg.match(/-->\s*\S*?:(\d+):(\d+)/) ?? msg.match(/:(\d+):(\d+)/);
  return m ? { line: parseInt(m[1], 10), col: parseInt(m[2], 10) } : null;
};

// Jump the source editor to the span a diagnostic points at and flash
// the line so the eye catches the move. Caller is responsible for
// passing a (line, col) pair that's already bounds-checked against the
// current doc.
const jumpToLoc = (loc) => {
  const doc = view.state.doc;
  if (loc.line < 1 || loc.line > doc.lines) return;
  const lineInfo = doc.line(loc.line);
  const pos = Math.min(lineInfo.from + Math.max(0, loc.col - 1), lineInfo.to);
  view.dispatch({
    selection: { anchor: pos },
    effects: EditorView.scrollIntoView(pos, { y: 'center' }),
  });
  view.focus();
  // Temporary line-background flash so the user sees *which* line the
  // list item jumped to even when the cursor is off-screen. Reuses the
  // errorLineField StateField by overlaying a short-lived decoration
  // then clearing back to the post-parse set after a beat.
  const flash = Decoration.line({ attributes: { class: 'cm-diag-flash' } }).range(lineInfo.from);
  view.dispatch({ effects: setErrorLines.of([flash]) });
  setTimeout(() => updateErrorDecorations(), 600);
};

// Under the verdict we list each diagnostic as its own block. Messages
// whose line:col we can parse become clickable — click jumps the source
// editor to that span and opens the matching inline-lint tooltip,
// giving the bottom list and the in-editor squiggle a shared hand-off
// point (useful for expand-errors notes that span many lines).
// Format a per-stage duration. Sub-second readings get `ms` resolution
// (pipeline is noisy in the tens of ms); anything over 1s gets `1.2s`
// so the eye grasps the order of magnitude without scanning digits.
const fmtMs = (ms) => ms >= 1000 ? `${(ms / 1000).toFixed(1)}s` : `${Math.round(ms)}ms`;
const renderMeta = () => {
  parseMeta.textContent = '';
  if (verdictCache !== null) {
    const [headline, ...rest] = verdictCache.split('\n');
    const isOk = headline === 'verified';
    const div = document.createElement('div');
    div.className = `verdict ${isOk ? 'ok' : 'bad'}`;
    const status = document.createElement('div');
    status.className = `verdict-status ${isOk ? 'ok' : 'bad'}`;
    // Count errors / warnings straight off the unified diagnostic list
    // so the verdict headline carries a scan-friendly severity tally.
    // `note` / `help` entries are ignored — they're additive context,
    // not distinct findings.
    const errN = diagnostics.filter(d => d.severity === 'error').length;
    const warnN = diagnostics.filter(d => d.severity === 'warning').length;
    const counts = [];
    if (errN) counts.push(`${errN} ${errN === 1 ? 'error' : 'errors'}`);
    if (warnN) counts.push(`${warnN} ${warnN === 1 ? 'warning' : 'warnings'}`);
    const countSuffix = counts.length ? ` · ${counts.join(' · ')}` : '';
    status.textContent = (isOk ? '✓ verified' : `✗ ${headline}`) + countSuffix;
    div.appendChild(status);
    const detail = rest.join('\n').trim();
    if (detail) {
      const d = document.createElement('div');
      d.className = 'verdict-detail';
      d.textContent = detail;
      div.appendChild(d);
    }
    parseMeta.appendChild(div);
  }
  for (const d of diagnostics) {
    const pre = document.createElement('pre');
    pre.className = `diagnostic ${d.severity}`;
    // Rustc's JSON `rendered` always ends in `\n` (stdout-streaming
    // convention). Inside a `<pre>` that shows as a trailing blank
    // row, so strip before rendering.
    pre.textContent = d.rendered.replace(/\n+$/, '');
    if (d.loc && d.loc.line >= 1 && d.loc.line <= view.state.doc.lines) {
      pre.classList.add('clickable');
      pre.addEventListener('click', () => jumpToLoc(d.loc));
    }
    parseMeta.appendChild(pre);
  }
};

// Top tab strip: one button per pipeline stage. A group is enabled as
// long as *any* of its variants has produced output. Clicking a group
// restores the last-visited variant within it (or falls back to the
// first cached one) so flipping away and back doesn't lose context.
// Stages not yet cached render as disabled buttons so the full pipeline
// shape stays visible during cold load / between runs.
const renderTabs = () => {
  // Remove old tab buttons but preserve the static `.tab-actions`
  // wrapper (and its bound click handlers) that lives in markup.
  parseTabsEl.querySelectorAll('.tab').forEach(el => el.remove());
  const tabActions = parseTabsEl.querySelector('.tab-actions');
  const currentGroup = currentTab ? GROUP_OF.get(currentTab) : null;
  const cached = (v) => sectionCache.has(v);
  for (const g of TAB_GROUPS) {
    const button = document.createElement('button');
    button.type = 'button';
    button.className = 'tab' + (g.id === currentGroup ? ' active' : '');
    button.textContent = g.label;
    button.disabled = !g.variants.some(cached);
    button.addEventListener('click', () => {
      const chosen = (lastVariantInGroup.has(g.id) && cached(lastVariantInGroup.get(g.id)))
        ? lastVariantInGroup.get(g.id)
        : g.variants.find(cached);
      if (!chosen) return;
      currentTab = chosen;
      lastVariantInGroup.set(g.id, chosen);
      rerender();
      writeTabToUrl();
    });
    parseTabsEl.insertBefore(button, tabActions);
  }
  downloadBtn.disabled = !currentTab || !sectionCache.has(currentTab);
  downloadBtn.title = currentTab
    ? `Download the ${currentTab} tab's contents`
    : 'Download the current IR tab';
};

// Subtab row: sits right under the top tab bar, above the CM6 view.
// One pill per sibling variant in the active group (e.g. `ast` /
// `sst` / `poly` under VIR). Skipped when the group has a single
// variant (Verus, HIR). Hidden via `.output-subtabs:empty` when the
// row would otherwise render nothing, so tabs with a single variant
// don't leave a dead band above the editor. (VSTD items now ride
// along inside each variant's body as a foldable block — see the
// `fold: true` flag in `verus_dump` payloads — so there's no
// separate Show-vstd toggle here.)
const renderSubtabs = () => {
  parseSubtabsEl.textContent = '';
  const currentGroup = currentTab ? GROUP_OF.get(currentTab) : null;
  if (!currentGroup) return;
  const group = TAB_GROUPS.find(g => g.id === currentGroup);
  if (group.variants.length > 1) {
    for (const v of group.variants) {
      const button = document.createElement('button');
      button.type = 'button';
      button.className = 'subtab' + (v === currentTab ? ' active' : '');
      button.textContent = VARIANT_LABEL[v];
      button.disabled = !sectionCache.has(v);
      button.addEventListener('click', () => {
        currentTab = v;
        lastVariantInGroup.set(currentGroup, v);
        rerender();
        writeTabToUrl();
      });
      parseSubtabsEl.appendChild(button);
    }
  }
  // Right-of-subtabs status line: `stageMs / totalMs` — this tab's
  // stage vs the full pipeline, so proportion is visible at a glance.
  const benchBucket = TAB_GROUP_BENCH[currentGroup];
  let stageMs = 0, totalMs = 0;
  for (const [label, v] of benchCache) {
    const g = BENCH_GROUP[label];
    if (!g) continue;
    totalMs += v;
    if (g === benchBucket) stageMs += v;
  }
  if (totalMs > 0) {
    const timing = document.createElement('div');
    timing.className = 'subtab-timing';
    timing.textContent = `${fmtMs(stageMs)} / ${fmtMs(totalMs)}`;
    parseSubtabsEl.appendChild(timing);
  }
};

// Minimal s-expression stream parser for the AIR / SMT / Z3 tabs.
// Groups SMT-LIB tokens into a handful of CM6 tag buckets so the
// theme can give each its own color: commands & connectives →
// keyword, built-in sorts → typeName, booleans → atom, arithmetic /
// comparison / bitvec / string / array ops → operator, `:foo`
// attribute keywords → attributeName. Unknown tokens fall through
// as plain identifiers.
// Non-head constants — things that appear as *arguments*, not as the
// first token after `(`, and so can't be caught by the head-position
// fallback in the tokenizer. Shared across SMT and VIR since both
// can emit these (booleans ubiquitously, Z3 reply atoms at top level
// of the response tab, IEEE rounding modes as args to `fp.*` ops).
// Add here whenever you see a bare non-head identifier that should
// color as a constant rather than a plain variable.
const SEXP_ATOMS = new Set([
  'true', 'false',
  'sat', 'unsat', 'unknown',            // Z3 `(check-sat)` / `(get-*)` replies
  'RNA', 'RNE', 'RTN', 'RTP', 'RTZ',    // IEEE-754 rounding modes
]);
// Shared tokenizer for every parenthesized IR tab (AIR, SMT, Z3, VIR,
// SST). Two things carry coloring:
//   - `SEXP_ATOMS`: non-head constants (booleans, Z3 replies, FP
//     rounding modes).
//   - head-position rule: the first atom after `(` is colored as
//     keyword — catches every command / node kind / operator head
//     (declare-fun, assert, axiom, fuel_bool, Function, BinaryOp, …)
//     without anyone having to enumerate them.
// Everything else — variable names, type references like `Bool` /
// `Int` in argument position — falls through as a plain identifier.
// Stream state tracks `afterOpen`: reset on `)`, strings, `|quoted|`
// idents, numbers; preserved through whitespace and comments.
const sexpParser = {
  name: 'sexp',
  startState: () => ({ afterOpen: false }),
  token(stream, state) {
    if (stream.eatSpace()) return null;
    const ch = stream.peek();
    if (ch === ';') { stream.skipToEnd(); return 'comment'; }
    if (ch === '"') {
      stream.next();
      while (!stream.eol()) {
        const c = stream.next();
        if (c === '\\') { stream.next(); continue; }
        if (c === '"') break;
      }
      state.afterOpen = false;
      return 'string';
    }
    // SMT-LIB allows arbitrary characters inside `|...|` — eat to the
    // closing bar so internal whitespace / parens don't break tokenization.
    if (ch === '|') {
      stream.next();
      while (!stream.eol()) { if (stream.next() === '|') break; }
      state.afterOpen = false;
      return 'variableName';
    }
    if (ch === '(') { stream.next(); state.afterOpen = true;  return 'bracket'; }
    if (ch === ')') { stream.next(); state.afterOpen = false; return 'bracket'; }
    const head = state.afterOpen;
    state.afterOpen = false;
    if (/[0-9]/.test(ch)) { stream.eatWhile(/[\w.]/); return 'number'; }
    stream.eatWhile(/[^\s()"';|]/);
    const w = stream.current();
    // `:foo` is an SMT-LIB attribute keyword (`:named`, `:pattern`,
    // `:weight`, …). Render distinctly from command keywords.
    if (w.startsWith(':')) return 'attributeName';
    if (SEXP_ATOMS.has(w)) return 'atom';
    if (head) return 'keyword';
    return null;
  },
};
const sexpLanguage = StreamLanguage.define(sexpParser);
// Per-section language, looked up on every tab switch. Sections not
// listed get `[]` (plain). Rust mode for AST/HIR since those are
// pseudo-Rust pretty-prints; s-expressions for everything downstream
// (VIR/SST also pretty-print as parenthesized trees, close enough).
const LANGUAGE_FOR_SECTION = {
  AST_PRE: rust(), AST: rust(), HIR: rust(), HIR_TYPED: rust(),
  VIR: sexpLanguage, SST_AST: sexpLanguage, SST_POLY: sexpLanguage,
  AIR_INITIAL: sexpLanguage, AIR_MIDDLE: sexpLanguage, AIR_FINAL: sexpLanguage,
  SMT: sexpLanguage, Z3: sexpLanguage,
};
// Compartment lets us hot-swap the output editor's language on tab
// flips without re-creating the view (which would drop scroll + lose
// the search panel's state).
const outputLanguage = new Compartment();

// Fold service — basicSetup's foldGutter asks this per line; non-
// foldable lines return null. All foldable ranges are Rust-declared
// (VIR user/vstd split, AIR/SMT prelude + per-query blocks) and
// stored in `sectionFolds`: JS never scans the body to decide what
// folds. `renderOutputView` applies the same ranges on initial
// render; this service just re-offers them so the user can refold
// after expanding.
const sectionFold = foldService.of((state, lineStart, lineEnd) => {
  for (const f of sectionFolds.get(currentTab) ?? []) {
    if (f.from === lineEnd) return f;
  }
  return null;
});

// Style `;; …` comment lines as section banners (`.cm-banner-line`).
// Scans only the visible range each update; stream-lang already colors
// the tokens, this just paints the whole line as a header strip.
const bannerLines = ViewPlugin.fromClass(class {
  constructor(view) { this.decorations = this.compute(view); }
  update(u) {
    if (u.docChanged || u.viewportChanged) this.decorations = this.compute(u.view);
  }
  compute(view) {
    const b = new RangeSetBuilder();
    for (const { from, to } of view.visibleRanges) {
      let pos = from;
      while (pos <= to) {
        const line = view.state.doc.lineAt(pos);
        if (line.text.startsWith(';;')) {
          b.add(line.from, line.from, Decoration.line({ class: 'cm-banner-line' }));
        }
        if (line.to >= to) break;
        pos = line.to + 1;
      }
    }
    return b.finish();
  }
}, { decorations: v => v.decorations });

// Mark `input.rs:L:C` occurrences as clickable links
// (`.cm-span-link`). Mousedown dispatches `jumpToLoc` — the same
// path the diagnostic list uses — turning output-view spans into
// a second hand-off surface for navigating back to the source.
// `FileName::Custom("input.rs")` formats as `<input.rs>` in rustc span
// Debug output, so the match must accept the surrounding angle brackets.
// Covers the full `<input.rs>:L:C: L2:C2 (#N)` form so the whole
// reference becomes one clickable mark; line/col come from the start.
const SPAN_LINK_RE = /<?input\.rs>?:(\d+):(\d+)(?:: \d+:\d+)?(?: \(#\d+\))?/g;
const spanLinks = ViewPlugin.fromClass(class {
  constructor(view) { this.decorations = this.compute(view); }
  update(u) {
    if (u.docChanged || u.viewportChanged) this.decorations = this.compute(u.view);
  }
  compute(view) {
    const b = new RangeSetBuilder();
    for (const { from, to } of view.visibleRanges) {
      const text = view.state.doc.sliceString(from, to);
      SPAN_LINK_RE.lastIndex = 0;
      let m;
      while ((m = SPAN_LINK_RE.exec(text))) {
        const start = from + m.index;
        b.add(start, start + m[0].length, Decoration.mark({
          class: 'cm-span-link',
          attributes: { 'data-line': m[1], 'data-col': m[2], title: 'Jump to source' },
        }));
      }
    }
    return b.finish();
  }
}, {
  decorations: v => v.decorations,
  eventHandlers: {
    mousedown(e) {
      // CM6's stream-language tokenizer wraps sub-spans inside the
      // mark decoration, so `e.target` is usually an inner token
      // element. Walk up to the `.cm-span-link` ancestor to read the
      // data-line / data-col attributes placed on the mark itself.
      const el = e.target instanceof Element ? e.target.closest('.cm-span-link') : null;
      if (!el) return;
      const line = parseInt(el.dataset.line, 10);
      const col = parseInt(el.dataset.col, 10);
      if (Number.isFinite(line) && Number.isFinite(col)) {
        jumpToLoc({ line, col });
        e.preventDefault();
      }
    },
  },
});

// Replace the read-only CM6 viewer's doc with the selected section body,
// then restore the user's last scroll position for that tab (0 on first
// visit). One long-lived view is cheaper than N-per-tab; the scroll map
// is what buys back the "pick up where I left off" feel.
//
// The whole-doc replacement wipes any prior fold state (folds over a
// deleted range collapse to zero width), so Rust-declared folds are
// re-applied from `sectionFolds` on every render. JS owns no body
// scanning — every fold range came from `verus_dump`.
const renderOutputView = () => {
  const body = currentTab && sectionCache.has(currentTab) ? sectionCache.get(currentTab) : '';
  outputView.dispatch({
    changes: { from: 0, to: outputView.state.doc.length, insert: body },
    effects: outputLanguage.reconfigure(LANGUAGE_FOR_SECTION[currentTab] ?? []),
  });
  for (const { from, to } of sectionFolds.get(currentTab) ?? []) {
    outputView.dispatch({ effects: foldEffect.of({ from, to }) });
  }
  outputView.scrollDOM.scrollTop = tabScrolls.get(currentTab) ?? 0;
};

// Full re-render after a state change that can affect tabs, subtabs,
// and the shown section (tab click, subtab click, new parse result).
// All three are cheap (pure DOM rebuild + doc swap), so we always run
// the set rather than tracking dirty bits.
const rerender = () => { renderTabs(); renderSubtabs(); renderOutputView(); };

// Build CM6 `Diagnostic` objects (the @codemirror/lint shape) from the
// structured JsonEmitter payloads — one per primary span, with
// rustc-exact widths and level. Falls back to the text-only cache when
// no JSON arrived (e.g., a message routed through a non-rustc path).
// Yields the hover tooltip + squiggle underline in the editor, in sync
// with the line-bg wash `updateErrorDecorations` applies.
//
// Pure: reads from the diagnostic caches and a given `doc`, returns a
// fresh array. Used both by the post-parse dispatch in `runParse` and
// by the `linter(...)` source below — the latter re-fires on every
// doc change, so returning the cached diagnostics (instead of `[]`)
// keeps the lint field idempotent across those auto-firings.
const computeInlineDiagnostics = (doc) => {
  const diags = [];
  // The wasm side feeds a single `input.rs` through `Input::Str`, so
  // every span's `file_name` is that virtual path. Use `line` + `col`
  // to map into the CM6 doc: JS strings are UTF-16 and rustc's
  // `column_start` is a 1-based character offset, so walking from
  // the line start by `col - 1` characters lines the squiggle up on
  // the exact grapheme rustc caret'd. (Using byte offsets directly
  // would desync on any non-ASCII source byte.)
  const toOffset = (line, col) => {
    if (line < 1 || line > doc.lines) return null;
    const info = doc.line(line);
    return Math.min(info.from + Math.max(0, col - 1), info.to);
  };
  for (const d of diagnostics) {
    if (!d.loc) continue;
    const from = toOffset(d.loc.line, d.loc.col);
    if (from === null) continue;
    // JSON-side entries carry an end span; text-fallback entries
    // don't, so widen the squiggle to end-of-line in that case.
    let to;
    if (d.loc.endLine !== undefined) {
      to = toOffset(d.loc.endLine, d.loc.endCol);
      if (to === null) continue;
      if (to <= from) to = Math.min(from + 1, doc.length);
    } else {
      to = doc.line(d.loc.line).to;
    }
    diags.push({ from, to, severity: d.severity, message: d.rendered });
  }
  // CM6 requires diagnostics sorted by `from`.
  diags.sort((a, b) => a.from - b.from);
  return diags;
};
const buildInlineDiagnostics = () => {
  view.dispatch(setDiagnostics(view.state, computeInlineDiagnostics(view.state.doc)));
};

// Convert diagnostic locations into line decorations on the source
// editor. Called post-parse so marks reflect the latest run; an edit
// afterwards leaves them in place (mapped through the change) until
// the next parse refreshes them.
const updateErrorDecorations = () => {
  const doc = view.state.doc;
  // CM6 requires decorations fed into a set to be sorted by `from`.
  // Collect with the line `from` first, sort, then materialize the
  // correct decoration per severity. Error wins when the same line has
  // both an error and a warning — two line-bg decorations on one line
  // both render and the warning would visually layer over the error.
  const byLine = new Map();
  for (const d of diagnostics) {
    if (!d.loc || d.severity === 'note') continue;
    if (d.loc.line < 1 || d.loc.line > doc.lines) continue;
    const from = doc.line(d.loc.line).from;
    const prev = byLine.get(from);
    if (!prev || (prev === 'warning' && d.severity === 'error')) {
      byLine.set(from, d.severity);
    }
  }
  const decos = [...byLine.entries()]
    .sort(([a], [b]) => a - b)
    .map(([from, sev]) => (sev === 'error' ? errorLineDeco : warningLineDeco).range(from));
  view.dispatch({ effects: setErrorLines.of(decos) });
};

// ------------------------------------------------------------------
// Rust → JS bridge. Synchronous during `parse_source`; JS is
// single-threaded, so the DOM won't actually repaint between these
// calls. On a mid-pipeline trap the buffered caches survive the
// unwind and `finally`-time renderers flush what's there.
// ------------------------------------------------------------------
globalThis.verus_diagnostic = (msg) => { _textDiags.push(msg); };
globalThis.verus_diagnostic_json = (msg) => {
  try { _jsonDiags.push(JSON.parse(msg)); }
  catch (e) { console.warn('verus_diagnostic_json: parse failed', e, msg); }
};
// Rust sends ordered blocks via parallel arrays: `contents[i]` is
// the block text (already includes a natural `;;` comment on its
// first line — `;; AIR prelude`, `;; Function-Def foo`, `;; vstd`,
// etc.), `folds[i] === 1` asks JS to auto-fold the block on render.
// Concatenate with `\n` between blocks; fold range is
// [end-of-first-line, end-of-block] so the natural comment line
// stays visible as the fold's label. `sectionFolds` feeds both the
// initial render (in `renderOutputView`) and the manual-refold path
// (in `sectionFold`).
globalThis.verus_dump = (section, contents, folds) => {
  let body = '';
  const ranges = [];
  for (let i = 0; i < contents.length; i++) {
    if (body.length > 0 && !body.endsWith('\n')) body += '\n';
    const start = body.length;
    body += contents[i];
    if (folds[i]) {
      // Banner-only blocks (e.g. empty-section `;; traits`) have no
      // newline and nothing to hide — skip the fold range, otherwise
      // `firstNl === -1` creates a negative-offset fold that CM6
      // renders as nested placeholders over adjacent banners.
      const firstNl = contents[i].indexOf('\n');
      if (firstNl >= 0) ranges.push({ from: start + firstNl, to: body.length });
    }
  }
  if (section === 'VERDICT') {
    verdictCache = body;
  } else {
    sectionCache.set(section, body);
    if (ranges.length > 0) sectionFolds.set(section, ranges);
  }
};
// Rust side calls `verus_bench(label, ms)` once per timed pipeline
// stage (one entry per `time(label, ||...)` wrapper in `src/lib.rs`).
// Accumulate rather than replace: `rustc_parse` for example fires
// inside nested `time()` calls, and the same label may repeat on
// cold / warm runs with slightly different breakdowns.
globalThis.verus_bench = (label, ms) => {
  benchCache.set(label, (benchCache.get(label) ?? 0) + ms);
};
// `run_queries` calls this once per op (and once for the prelude) so
// the Z3 tab — which otherwise concatenates Z3's raw replies into a
// positional stream — carries the same `;; Function-Def ...` banner
// headers as the SMT tab. A banner with no reply beneath it means
// the op only sent declarations / axioms (no `(check-sat)`), which
// is the common case for Context ops.
globalThis.verus_z3_annotate = (label) => {
  z3ResponseBuffer.push(`;; ${label}`);
};

// Declared here (before `runParse`) so `runParse` can cancel any
// pending auto-verify at its top — otherwise an explicit verify
// (Verify click, example load) races with a pending 500 ms auto-
// verify timer from a just-prior doc change and we end up double-
// verifying. The timer itself is armed by
// `scheduleAutoVerify` further down.
let autoVerifyTimer;
let runId = 0;
const runParse = async () => {
  // Wasm not ready yet — silently no-op. Reached only through
  // `scheduleAutoVerify` firing during cold-load (the Verify button
  // is disabled until `wasmReady` resolves, so the click path can't
  // land here). The final `await wasmReady` at the bottom of this
  // script runs the first real verify once wasm is live.
  if (!verus) return;
  clearTimeout(autoVerifyTimer);
  // Flush any debounced URL-hash save so example-switch / Verify
  // coalesce the dispatch-armed save into a single `replaceState`.
  flushHashSave();
  const myRun = ++runId;
  parseButton.disabled = true;
  parseButtonLabel.textContent = 'Verifying…';
  sectionCache.clear();
  sectionFolds.clear();
  verdictCache = null;
  _textDiags.length = 0;
  _jsonDiags.length = 0;
  diagnostics.length = 0;
  benchCache.clear();
  z3ResponseBuffer = [];
  // Yield to the browser so the disabled button + "Verifying…" label
  // actually paint before `parse_source` pegs the main thread. rAF
  // schedules the callback for the next pre-paint hook; the nested
  // `setTimeout(_, 0)` defers the resume until *after* that paint has
  // committed. Without this, the DOM mutations above stay invisible —
  // the sync wasm call runs to completion, then the `finally` below
  // flips the button back to "Verify" before any frame lands.
  await new Promise(r => requestAnimationFrame(() => setTimeout(r, 0)));
  // If a newer runParse fired while we were yielding (rapid ⌘↵ mashing,
  // or auto-verify firing during an explicit click), abandon this one
  // — the later one will do its own fresh work.
  if (myRun !== runId) return;
  try {
    verus.parse_source(view.state.doc.toString(), expandErrorsCheckbox.checked);
  } catch (e) {
    if (myRun !== runId) return;
    if (_textDiags.length === 0 && sectionCache.size === 0 && verdictCache === null) {
      _textDiags.push('Parse crashed: ' + (e?.message ?? e));
      console.error(e);
    }
  } finally {
    if (myRun === runId) {
      parseButton.disabled = false;
      parseButtonLabel.textContent = 'Verify';
    }
  }
  // Z3 transcript lives on the JS side (wrapped cwrap), so it's installed
  // after `parse_source` returns rather than via `verus_dump`.
  if (z3ResponseBuffer.length) {
    sectionCache.set('Z3', z3ResponseBuffer.join('\n'));
  }
  // Consolidate the raw emission channels into the unified diagnostic
  // list. JSON wins when rustc produced any (rustc-exact spans + level
  // tags); otherwise fall back to the human channel so Verus-side or
  // crash messages still surface. The two channels describe the same
  // set of diagnostics, so mixing would double-count.
  if (_jsonDiags.length > 0) {
    for (const j of _jsonDiags) {
      // Skip rustc's "aborting due to N previous errors" footer. It's
      // a summary of the preceding errors, not a distinct finding —
      // the text channel filters it in `emit_block` but the JSON
      // channel emits it, so it shows up here without this guard.
      if ((j.message ?? '').startsWith('aborting due to')) continue;
      const primary = j.spans?.find(s => s.is_primary) ?? j.spans?.[0];
      const loc = primary ? {
        line: primary.line_start, col: primary.column_start,
        endLine: primary.line_end, endCol: primary.column_end,
      } : null;
      const sev = j.level === 'warning' ? 'warning'
               : (j.level === 'note' || j.level === 'help') ? 'note'
               : 'error';
      diagnostics.push({ rendered: j.rendered ?? j.message ?? '', loc, severity: sev });
    }
  } else {
    for (const msg of _textDiags) {
      diagnostics.push({ rendered: msg, loc: extractLoc(msg), severity: severity(msg) });
    }
  }
  // Preserve user's tab selection when it survives the new run;
  // otherwise default to SMT (the actual solver input — most useful
  // for debugging why a query failed) or whichever stage made it
  // the furthest if SMT wasn't reached.
  if (!currentTab || !sectionCache.has(currentTab)) {
    currentTab = sectionCache.has('SMT') ? 'SMT'
      : (SECTION_ORDER.find(n => sectionCache.has(n)) ?? null);
  }
  if (currentTab) lastVariantInGroup.set(GROUP_OF.get(currentTab), currentTab);
  renderMeta();
  rerender();
  updateErrorDecorations();
  buildInlineDiagnostics();
  // Keep `&t=<TAB>` in sync when the default-pick logic above swapped
  // the tab (e.g. the shared link named a tab that isn't produced by
  // this source). Cheap — just rewrites the suffix.
  writeTabToUrl();
};

// ------------------------------------------------------------------
// URL hash sync: gzip + base64url-encode the doc into `location.hash`
// on every edit (debounced 500ms) so the address bar always reflects
// the current source and is directly shareable. Also decodes on page
// load (see `initialDoc` below) to restore from a pasted link.
// CompressionStream is Safari 16.4+ / Chrome 80+ / Firefox 113+.
// ------------------------------------------------------------------
const b64urlEncode = (bytes) => {
  let s = '';
  for (const b of bytes) s += String.fromCharCode(b);
  return btoa(s).replaceAll('+', '-').replaceAll('/', '_').replace(/=+$/, '');
};
const b64urlDecode = (str) => {
  const padded = str.replaceAll('-', '+').replaceAll('_', '/')
    + '='.repeat((4 - str.length % 4) % 4);
  const bin = atob(padded);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
};
const encodeSrc = async (src) => {
  const bytes = new TextEncoder().encode(src);
  const stream = new Blob([bytes]).stream().pipeThrough(new CompressionStream('gzip'));
  const compressed = new Uint8Array(await new Response(stream).arrayBuffer());
  return b64urlEncode(compressed);
};
const decodeSrc = async (hash) => {
  const stream = new Blob([b64urlDecode(hash)]).stream().pipeThrough(new DecompressionStream('gzip'));
  return await new Response(stream).text();
};
// Debounced URL writer. `scheduleHashSave` is called from the
// updateListener on every doc change; `flushHashSave` is called at
// the top of `runParse` so a Verify / example-switch coalesces the
// dispatch-armed save into a single `replaceState` (no double
// update). Copy-link also awaits `flushHashSave` before reading
// `location.href` so the clipboard is never stale.
//
// Generation guard: `saveHashNow` captures `++hashSaveGen` and
// skips its `replaceState` if a later call bumped the gen past
// it — prevents a slow in-flight encode from overwriting a newer
// one when saves overlap.
//
// Hash layout (three shapes; `&t=<TAB>` is optional on all three):
//   * `#source=<file>[&t=<TAB>]`  — shipped example, unmodified.
//     Shortest form; preferred when the editor matches `pristineSource`.
//   * `#code=<b64>[&t=<TAB>]`      — arbitrary source (current form for
//     modified examples / user-pasted snippets / typed-from-scratch docs).
//   * `#<b64>[&t=<TAB>]`           — legacy bare-b64 form; still parsed
//     on load so old shared links keep working, but never written.
// `parseHash` returns the same shape whichever form it saw, so the
// hash-load path doesn't branch on layout.
let hashSaveTimer = null;
let hashSaveGen = 0;
// The last hash we wrote via `replaceState`, tracked so the
// `hashchange` listener can distinguish our own writes from the
// user editing the URL bar. `history.replaceState` doesn't fire
// hashchange in compliant browsers, but this guard keeps the
// behavior stable if a code path ever flips to `location.hash = …`.
let lastWrittenHash = location.hash;
const buildHashTabSuffix = () => currentTab ? `&t=${currentTab}` : '';
// Pristine shipped example → short-link form. Otherwise re-encode
// the doc. `pristineSource` is known at this point (set during
// init / loadSource before any save fires).
const buildHash = async () => {
  if (loadedSource !== null && !isDirty()) {
    return '#source=' + loadedSource + buildHashTabSuffix();
  }
  const encoded = await encodeSrc(view.state.doc.toString());
  return '#code=' + encoded + buildHashTabSuffix();
};
const writeHash = (h) => {
  lastWrittenHash = h;
  history.replaceState(null, '', h);
};
const saveHashNow = async () => {
  const myGen = ++hashSaveGen;
  const h = await buildHash();
  if (myGen !== hashSaveGen) return;
  writeHash(h);
};
// Rewrite only the `&t=` suffix without re-encoding the source —
// cheap enough to call on every tab click. Preserves whichever of
// the three hash shapes the current hash uses.
const writeTabToUrl = () => {
  const hash = location.hash.slice(1);
  if (!hash) return;
  const head = hash.split('&')[0];
  if (!head) return;
  writeHash('#' + head + buildHashTabSuffix());
};
const scheduleHashSave = () => {
  if (hashSaveTimer) clearTimeout(hashSaveTimer);
  hashSaveTimer = setTimeout(() => {
    hashSaveTimer = null;
    saveHashNow();
  }, 500);
};
const flushHashSave = () => {
  if (!hashSaveTimer) return Promise.resolve();
  clearTimeout(hashSaveTimer);
  hashSaveTimer = null;
  return saveHashNow();
};
// Parse a hash string (without the leading `#`) and return
//   { sourceFile, src, tab } | null
// where exactly one of `sourceFile` / `src` is non-null on success,
// or `null` if parsing failed. Tab is echoed regardless of the
// head shape.
const parseHash = async (hash) => {
  if (!hash) return null;
  const [head, ...rest] = hash.split('&');
  let sourceFile = null;
  let src = null;
  let tab = null;
  try {
    if (head.startsWith('source=')) {
      const file = head.slice('source='.length);
      if (!optionByFile.has(file)) throw new Error('unknown example: ' + file);
      sourceFile = file;
    } else if (head.startsWith('code=')) {
      src = await decodeSrc(head.slice('code='.length));
    } else {
      // Legacy bare-b64 form.
      src = await decodeSrc(head);
    }
  } catch (e) {
    console.warn('hash decode failed:', e);
    return null;
  }
  for (const kv of rest) {
    if (kv.startsWith('t=')) {
      const t = kv.slice(2);
      if (SECTION_ORDER.includes(t)) tab = t;
    }
  }
  return { sourceFile, src, tab };
};

// Filename extension per IR tab, used by the Download button. The
// AIR / SMT / Z3 stages are all SMT-LIB2-shaped; AST / HIR are
// pseudo-Rust; VIR / SST are their own s-expression-ish form, so
// `.vir` keeps editors from assuming Rust syntax.
const EXT_FOR_TAB = {
  AST_PRE: 'rs', AST: 'rs', HIR: 'rs', HIR_TYPED: 'rs',
  VIR: 'vir', SST_AST: 'vir', SST_POLY: 'vir',
  AIR_INITIAL: 'smt2', AIR_MIDDLE: 'smt2', AIR_FINAL: 'smt2',
  SMT: 'smt2', Z3: 'smt2',
};
// `rust-ir-expanded-ast.rs` reads better than `verus-AST.rs`. Joins
// the group label and the variant label (both user-facing) as
// lowercase hyphen-separated slugs. Group prefix disambiguates
// collisions like VIR's "AST" subtab vs. Rust IR's "AST" subtab.
const slug = s => s.toLowerCase().replace(/\s+/g, '-');
const downloadCurrentTab = () => {
  if (!currentTab || !sectionCache.has(currentTab)) return;
  const body = sectionCache.get(currentTab);
  const ext = EXT_FOR_TAB[currentTab] ?? 'txt';
  const group = TAB_GROUPS.find(g => g.variants.includes(currentTab));
  const name = `${slug(group.label)}-${slug(VARIANT_LABEL[currentTab] ?? currentTab)}`;
  const blob = new Blob([body], { type: 'text/plain;charset=utf-8' });
  const url = URL.createObjectURL(blob);
  const a = document.createElement('a');
  a.href = url;
  a.download = `${name}.${ext}`;
  document.body.appendChild(a);
  a.click();
  a.remove();
  URL.revokeObjectURL(url);
};
const copyLink = async (button) => {
  // Always save the current doc before copying — cold load with the
  // default example has no hash yet, so a plain `flushHashSave` would
  // copy a bare URL that restores the default rather than whatever's
  // on screen. Cancel any pending debounce so it doesn't re-fire
  // redundantly after this explicit save.
  if (hashSaveTimer) { clearTimeout(hashSaveTimer); hashSaveTimer = null; }
  try {
    await saveHashNow();
    await navigator.clipboard.writeText(location.href);
    const prev = button.textContent;
    button.textContent = 'Copied ✓';
    setTimeout(() => { button.textContent = prev; }, 1500);
  } catch (e) {
    console.warn('copy failed:', e);
  }
};
downloadBtn.addEventListener('click', downloadCurrentTab);
shareBtn.addEventListener('click', () => copyLink(shareBtn));

// Resolve the initial editor source from (in priority order):
//   1. `#source=<file>`  → shipped source + any localStorage override
//      for that file (the override wins; dirty marker is applied post-mount).
//   2. `#code=<b64>` / legacy `#<b64>` → decoded source, custom mode.
//   3. No hash → the first shipped example + any override for it.
// Restores `currentTab` from the hash suffix before the first
// `runParse` so the default-pick logic treats it as "keep this"
// when the named section is cached.
const initialDoc = await (async () => {
  const parsed = await parseHash(location.hash.slice(1));
  if (parsed?.tab) currentTab = parsed.tab;
  if (parsed && parsed.sourceFile) {
    loadedSource = parsed.sourceFile;
    pristineSource = await fetchExample(parsed.sourceFile);
    const stored = localStorage.getItem(STORAGE_PREFIX + parsed.sourceFile);
    return stored !== null && stored !== pristineSource ? stored : pristineSource;
  }
  if (parsed && parsed.src !== null) {
    // Custom content — no shipped example to diff against.
    return parsed.src;
  }
  // Cold load with no hash → first example as the default.
  const first = EXAMPLES[0].file;
  loadedSource = first;
  pristineSource = await fetchExample(first);
  const stored = localStorage.getItem(STORAGE_PREFIX + first);
  return stored !== null && stored !== pristineSource ? stored : pristineSource;
})();

// ------------------------------------------------------------------
// Auto-verify: debounced re-run on edits when the checkbox is on.
// Setting is persisted in localStorage so the preference survives a
// reload. 500ms feels long enough that fast typing doesn't burn
// verifier cycles but short enough that a brief pause fires a check.
// ------------------------------------------------------------------
const AUTO_VERIFY_KEY = 've-auto-verify';
// Default on — unchecked only if the user has explicitly turned it off.
autoVerifyCheckbox.checked = localStorage.getItem(AUTO_VERIFY_KEY) !== 'false';
autoVerifyCheckbox.addEventListener('change', () => {
  localStorage.setItem(AUTO_VERIFY_KEY, autoVerifyCheckbox.checked);
  // Flipping the checkbox on is effectively a request to verify now —
  // the user just expressed "yes, I want verification running" — so
  // kick off a run. Flipping off is silent; we only cancel pending work.
  if (autoVerifyCheckbox.checked) runParse();
});
// Expand-errors: always default off — runs per-conjunct sub-queries on a
// failed body, which doubles-to-triples verify time but pinpoints which
// sub-assertion broke. Not persisted: starting clean each page load keeps
// the slower path opt-in rather than sticky.
expandErrorsCheckbox.checked = false;
expandErrorsCheckbox.addEventListener('change', () => {
  // The flag is only read by `parse_source`, so a re-run is required to
  // see the effect — do it eagerly instead of waiting for the next edit
  // or manual Verify click.
  runParse();
});
// `autoVerifyTimer` is declared up beside `runParse` so the explicit
// verify path can preempt a pending auto-fire; this function just
// arms it after each doc change.
const scheduleAutoVerify = () => {
  clearTimeout(autoVerifyTimer);
  if (!autoVerifyCheckbox.checked) return;
  autoVerifyTimer = setTimeout(runParse, 500);
};

// ------------------------------------------------------------------
// CM6 source editor. `oneDark` only applied when the user's system is
// in dark mode; everything else keys off CSS vars.
// ------------------------------------------------------------------
const dark = matchMedia('(prefers-color-scheme: dark)').matches;
const view = new EditorView({
  parent: document.getElementById('parse-input'),
  doc: initialDoc,
  extensions: [
    basicSetup,
    rust(),
    ...(dark ? [oneDark] : []),
    errorLineField,
    // Pin the search panel to the top so Cmd+F opens it in a spot that
    // isn't clipped by the flex container; basicSetup's searchKeymap
    // already binds Mod-f, we just need the panel installed.
    search({ top: true }),
    // Source recomputes from the current diagnostic caches so the
    // auto-firing on every doc change (default 750ms) is idempotent
    // instead of wiping the squiggles that `runParse` just set. The
    // fresh squiggles after a parse still land via the direct
    // `setDiagnostics` dispatch in `buildInlineDiagnostics` — the
    // linter extension itself is what enables the hover tooltip.
    linter(v => computeInlineDiagnostics(v.state.doc)),
    EditorView.updateListener.of(u => {
      if (u.docChanged) {
        scheduleAutoVerify();
        scheduleHashSave();
        // Per-example persistence + dirty marker. When an example is
        // loaded we mirror the editor into localStorage so switching
        // away and back preserves edits; flipping back to pristine
        // drops the entry so we don't hoard stale copies.
        if (loadedSource !== null) {
          const src = view.state.doc.toString();
          if (src === pristineSource) {
            localStorage.removeItem(STORAGE_PREFIX + loadedSource);
          } else {
            localStorage.setItem(STORAGE_PREFIX + loadedSource, src);
          }
        }
        updateSourceUI();
      }
    }),
    keymap.of([
      indentWithTab,
    ]),
  ],
});
// Focus the source editor right after mount so typing works from
// keystroke one — no pre-click into the editor required.
view.focus();

// Second CM6 instance, read-only, hosts the selected IR stage's
// output. `basicSetup` gets us line numbers + folding + search for
// free, which matters when the SMT stage dump runs to thousands of
// lines. `outputLanguage.of([])` reserves a language slot we swap
// per tab via `renderOutputView`.
const outputView = new EditorView({
  parent: parseOutputViewEl,
  doc: '',
  extensions: [
    basicSetup,
    search({ top: true }),
    outputLanguage.of([]),
    sectionFold,
    bannerLines,
    spanLinks,
    // `readOnly` rejects edit transactions; `editable.of(false)`
    // would set `contenteditable=false` which breaks Select-All and
    // text selection on some browsers, so we skip it.
    EditorState.readOnly.of(true),
    // Identifier-shaped IR output is not English prose — disable the
    // browser's spellcheck to lose the red squiggles and save CPU
    // on multi-thousand-line dumps.
    EditorView.contentAttributes.of({ spellcheck: 'false' }),
    ...(dark ? [oneDark] : []),
  ],
});
// Keep `tabScrolls` in sync with live user scrolling. Fires on our
// own programmatic `scrollTop = …` too, which just re-saves the value
// we just wrote — idempotent, so no guard flag needed.
outputView.scrollDOM.addEventListener('scroll', () => {
  if (currentTab) tabScrolls.set(currentTab, outputView.scrollDOM.scrollTop);
});

// ------------------------------------------------------------------
// Wiring: example dropdown + Verify button.
// ------------------------------------------------------------------
// Replace the editor doc wholesale. Treated as a programmatic
// transaction (not tagged as `input`/`delete`), so the updateListener's
// dirty-tracking logic still runs but the transaction can be told
// apart from real user edits if we ever need to again.
const setEditorText = (src) => {
  view.dispatch({ changes: { from: 0, to: view.state.doc.length, insert: src } });
};
// Load a shipped example by filename. Uses any localStorage override
// for that file so navigating away and back preserves edits; the
// dirty marker then flags the difference. Always triggers a verify.
const loadSource = async (file) => {
  if (!optionByFile.has(file)) return;
  pristineSource = await fetchExample(file);
  const stored = localStorage.getItem(STORAGE_PREFIX + file);
  const src = stored !== null && stored !== pristineSource ? stored : pristineSource;
  loadedSource = file;
  setEditorText(src);
  updateSourceUI();
  // Dispatch above already scheduled a hash save via the update
  // listener, but we want the address bar to reflect the new
  // example immediately (so Copy-link right after a switch doesn't
  // return the previous doc's hash).
  flushHashSave();
  runParse();
};
// Revert the editor to the shipped source and drop the localStorage
// override. Only meaningful while an example is loaded and dirty —
// the button is hidden otherwise.
const resetSource = () => {
  if (loadedSource === null) return;
  localStorage.removeItem(STORAGE_PREFIX + loadedSource);
  setEditorText(pristineSource);
  updateSourceUI();
  flushHashSave();
  runParse();
};
// Walk the flat `EXAMPLES` list by `step` (±1). From custom content,
// `step < 0` jumps to the last example and `step > 0` to the first,
// so the nav buttons always take the user somewhere useful.
const navSource = (step) => {
  const idx = loadedSource === null
    ? -1
    : EXAMPLES.findIndex(e => e.file === loadedSource);
  let next;
  if (idx < 0) next = step > 0 ? 0 : EXAMPLES.length - 1;
  else next = Math.max(0, Math.min(EXAMPLES.length - 1, idx + step));
  if (next !== idx) loadSource(EXAMPLES[next].file);
};
sourceSelect.addEventListener('change', () => {
  if (sourceSelect.value) loadSource(sourceSelect.value);
});
prevBtn.addEventListener('click', () => navSource(-1));
nextBtn.addEventListener('click', () => navSource(+1));
resetBtn.addEventListener('click', resetSource);
// Reflect the resolved initial state in the UI. Couldn't call this
// from the init block above because the view wasn't constructed
// yet and `isDirty` reads its doc.
updateSourceUI();
parseButton.addEventListener('click', runParse);
// External hash changes (user pastes a different link into the address
// bar, edits the fragment, hits back/forward on a history entry we
// wrote) should reload the editor. Our own `replaceState` calls don't
// fire hashchange in compliant browsers, but we also short-circuit
// on an exact match against `lastWrittenHash` as belt-and-braces.
window.addEventListener('hashchange', async () => {
  if (location.hash === lastWrittenHash) return;
  const parsed = await parseHash(location.hash.slice(1));
  if (parsed?.tab) currentTab = parsed.tab;
  if (parsed && parsed.sourceFile) {
    await loadSource(parsed.sourceFile);
    return;
  }
  if (parsed && parsed.src !== null) {
    loadedSource = null;
    pristineSource = null;
    setEditorText(parsed.src);
    updateSourceUI();
    flushHashSave();
    runParse();
    return;
  }
  // Empty / unparseable hash — fall back to the first example.
  loadSource(EXAMPLES[0].file);
});

// Paint the tab strip + empty output view immediately so the right
// pane shows its full structure (all tabs disabled) during cold load
// instead of a blank band.
renderTabs();
renderSubtabs();


// Hold here until the wasm chain resolves; editor + tabs are already
// live above so the left pane has been interactive the whole time.
await wasmReady;
parseButtonLabel.textContent = 'Verify';
parseButton.disabled = false;
runParse();
