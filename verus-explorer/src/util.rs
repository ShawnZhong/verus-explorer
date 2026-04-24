// Leaf helpers used by every pipeline stage: wall-clock timing, block /
// section emission, and the shared push_item / push_banner formatters
// that drive the auto-fold behavior of the browser's output view.

use std::sync::Arc;

use crate::externs::{perf_now, verus_bench, verus_dump};

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

// A chunk of content within a `Section`. The content's own first line
// is what stays visible when folded — so callers that want a visible
// label (AIR's `;; AIR prelude`, Verus's `;; Function-Def foo`, or the
// explorer-inserted `;; vstd` on VIR / SST) put it at the top of the
// content. `fold: true` asks JS to auto-collapse the block from that
// first line's end to end-of-content.
pub(crate) struct Block {
    pub(crate) content: String,
    pub(crate) fold: bool,
}

// An ordered list of `Block`s that together form one logical output tab.
// Most sections are a single `Block` with no fold; VIR / SST_AST /
// SST_POLY use two (vstd-with-`;; vstd` header + user) and AIR / SMT
// use many (prelude + one per op).
pub(crate) struct Section {
    pub(crate) name: &'static str,
    pub(crate) blocks: Vec<Block>,
}

impl Section {
    // Shorthand for the common single-block, no-fold case.
    pub(crate) fn single(name: &'static str, content: String) -> Self {
        Section { name, blocks: vec![Block { content, fold: false }] }
    }
}

// Streams a completed section to the browser via the `verus_dump` JS
// extern. Synchronous by design: a later stage that traps the wasm
// instance (rustc's `abort_if_errors` → `unreachable`) can't discard
// sections already handed off to JS. Callers of this crate (the browser
// and `tests/smoke.rs`) observe pipeline output exclusively through the
// JS callbacks — no String accumulator is threaded through.
pub(crate) fn emit_section(section: Section) {
    let n = section.blocks.len();
    let mut contents = Vec::with_capacity(n);
    let mut folds = Vec::with_capacity(n);
    for b in section.blocks {
        let mut c = b.content;
        c.truncate(c.trim_end().len());
        contents.push(c);
        folds.push(b.fold as u8);
    }
    verus_dump(section.name, contents, folds);
}

// Feed one per-item (crate, span, text) tuple from `walk_krate*` into
// `blocks`. Fold iff the item isn't local user code — external-crate
// items (`krate.is_some()`) and Verus-generated synthetics (`span ==
// "no location"`) collapse; items in the default crate stay expanded.
// Parallels the AIR/SMT drain rule in `run_queries`. Adjacent folded
// entries merge so vstd runs collapse into one row.
pub(crate) fn push_item(
    blocks: &mut Vec<Block>,
    krate: Option<Arc<String>>,
    span: &str,
    text: String,
) {
    let fold = krate.is_some() || span == "no location";
    let content =
        if span.is_empty() { text } else { format!(";; {}\n{}", span, text) };
    if let Some(prev) = blocks.last_mut() {
        if fold && prev.fold {
            prev.content.push('\n');
            prev.content.push_str(&content);
            return;
        }
    }
    blocks.push(Block { content, fold });
}

// Push a `;; <name>` section header that starts a fresh folded block.
// Subsequent `push_item` calls with `fold: true` merge into it so the
// banner sits at the top of the fold region and becomes the visible
// label when collapsed. Unlike `push_item`, never merges with the
// previous block — keeps each section's fold self-contained.
pub(crate) fn push_banner(blocks: &mut Vec<Block>, name: &str) {
    blocks.push(Block { content: format!(";; {}", name), fold: true });
}
