// Leaf helpers used by every pipeline stage: wall-clock timing, block /
// section emission, and the shared push_item / push_banner formatters
// that drive the auto-fold behavior of the browser's output view.

use std::sync::Arc;

use crate::wasm::{perf_now, verus_bench, verus_dump};

// Wrap a pipeline stage with a wall-clock timer. Result: one `verus_bench`
// call per stage, forwarded to console (browser) or stderr (smoke test).
// Kept synchronous + infallible so it composes cleanly around both closures
// and plain expressions; `perf_now` is a raw JS import so the overhead is
// two foreign calls per stage — negligible next to the stages themselves.
pub(crate) fn time<T>(label: &'static str, f: impl FnOnce() -> T) -> T {
    let t0 = perf_now();
    let result = f();
    verus_bench(label, perf_now() - t0);
    result
}

// Streams a completed section body to the browser as a single block
// via `verus_dump`. Fold structure is computed JS-side by scanning
// `;;>` / `;;v` / `;;<` markers embedded in the body — every banner-
// driven tab (VIR, SST_*, AIR_*, SMT_*) shares one scanner. Simpler
// tabs (VERDICT) pass an empty-marker body and render unfolded.
//
// Synchronous by design: a later stage that traps the wasm instance
// (rustc's `abort_if_errors` → `unreachable`) can't discard sections
// already handed off to JS. Callers observe pipeline output
// exclusively through the JS callbacks — no String accumulator is
// threaded through.
pub(crate) fn emit_section(name: &'static str, body: String) {
    let mut c = body;
    c.truncate(c.trim_end().len());
    verus_dump(name, vec![c], vec![0]);
}

// Buffered builder for walk-driven dumps (VIR / SST_AST / SST_POLY).
//
// Verus's `walk_krate` iterates the `Krate`'s per-kind fields
// (datatypes, then functions, then traits, …) with each field
// containing a mix of external-crate (vstd) and local-crate items.
// Streaming directly would interleave vstd and local within every
// kind. Instead we accumulate items here and stable-sort by crate
// at `finish()` so all of vstd's items appear together (regardless
// of kind), followed by local items. Within each crate, walk order
// is preserved — that keeps the per-kind grouping Verus emits.
//
// Each item becomes its own section:
//   `;;> <kind> <name> <span>`  — external (auto-folded)
//   `;;v <kind> <name> <span>`  — local (foldable, expanded)
//
// Runs of consecutive external items sharing the same (crate, kind)
// are wrapped in an outer `;;> <kind> <crate>` fold so the reader
// can collapse all of vstd's functions (or all of vstd's datatypes,
// etc.) in one click — and the outer banner's label tells them
// what's inside before they expand. Local items flow flat.
pub(crate) struct WalkBuilder {
    items: Vec<WalkItem>,
}

struct WalkItem {
    kind: &'static str,
    name: String,
    krate: Option<Arc<String>>,
    span: String,
    text: String,
}

impl WalkBuilder {
    pub(crate) fn new() -> Self {
        Self { items: Vec::new() }
    }

    pub(crate) fn add_item(
        &mut self,
        kind: &'static str,
        name: &str,
        krate: Option<Arc<String>>,
        span: &str,
        text: String,
    ) {
        self.items.push(WalkItem {
            kind,
            name: name.to_string(),
            krate,
            span: span.to_string(),
            text,
        });
    }

    pub(crate) fn finish(mut self) -> String {
        // Stable sort by crate — external crates first (Some), then
        // local (None). Within a crate, walk-emitted order stays.
        // `(is_none, krate)` key: `false` (is_some) < `true` (is_none)
        // puts externals first, ordered alphabetically by crate name
        // if there were multiple.
        self.items.sort_by(|a, b| {
            (a.krate.is_none(), a.krate.as_deref())
                .cmp(&(b.krate.is_none(), b.krate.as_deref()))
        });
        let mut body = String::new();
        let mut external_run: Option<(Arc<String>, &'static str)> = None;
        for item in self.items {
            let new_run = item.krate.as_ref().map(|k| (k.clone(), item.kind));
            if external_run != new_run {
                if external_run.is_some() {
                    body.push_str(";;<\n");
                }
                if let Some((k, _)) = &new_run {
                    body.push_str(";;> ");
                    body.push_str(item.kind);
                    body.push(' ');
                    body.push_str(k);
                    body.push('\n');
                }
                external_run = new_run;
            }
            let marker = if item.krate.is_some() { '>' } else { 'v' };
            body.push_str(";;");
            body.push(marker);
            body.push(' ');
            body.push_str(item.kind);
            body.push(' ');
            body.push_str(&item.name);
            if !item.span.is_empty() {
                body.push(' ');
                body.push_str(&item.span);
            }
            body.push('\n');
            body.push_str(&item.text);
            if !body.ends_with('\n') {
                body.push('\n');
            }
            body.push_str(";;<\n");
        }
        if external_run.is_some() {
            body.push_str(";;<\n");
        }
        body
    }
}
