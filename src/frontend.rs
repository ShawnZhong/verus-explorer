// Thin wrappers around rustc_lexer + rustc_arena + rustc_index — the first
// three rustc-internal crates we've pulled in. Proves the vendored
// `third_party/rust/compiler/rustc_*` path-dep story compiles to wasm32
// end-to-end, including a proc-macro dep (rustc_index_macros). All three crates
// are leaf-level (no threads/fs/process); `rustc_span` / `rustc_data_structures`
// is where real patching work would start.

use std::fmt::Write as _;

use rustc_arena::DroplessArena;
use rustc_index::{IndexVec, newtype_index};
use rustc_lexer::{FrontmatterAllowed, TokenKind, tokenize};

newtype_index! {
    struct TokenId {}
}

/// Tokenize `src` and return a newline-delimited dump:
///
///     <start>..<end>  <TokenKind>  <slice?>
///
/// `slice` is included only for tokens where the kind alone doesn't identify
/// the text (idents, literals, lifetimes, unknown). For punctuation and
/// whitespace the slice is redundant and just makes the output noisier.
///
/// Appends a `unique idents (DroplessArena)` summary line — the arena allocates
/// each identifier's text once (ignoring duplicates), so this is also the arena
/// smoke test: if rustc_arena wasn't alive in wasm, this function would not
/// link.
pub fn lex_source(src: &str) -> String {
    use TokenKind::*;
    let arena = DroplessArena::default();
    let mut idents: Vec<&str> = Vec::new();
    let mut tokens: IndexVec<TokenId, &str> = IndexVec::new();

    let mut out = String::new();
    let mut offset: u32 = 0;
    for tok in tokenize(src, FrontmatterAllowed::No) {
        let start = offset as usize;
        let end = start + tok.len as usize;
        let slice = &src[start..end];
        let id = tokens.push(arena.alloc_str(slice));
        let show_slice = matches!(
            tok.kind,
            Ident
                | InvalidIdent
                | RawIdent
                | UnknownPrefix
                | UnknownPrefixLifetime
                | RawLifetime
                | Literal { .. }
                | Lifetime { .. }
                | Unknown
                | Frontmatter { .. }
        );
        if show_slice {
            writeln!(
                out,
                "{id:?} {start:>4}..{end:<4} {:?}  {:?}",
                tok.kind, slice
            )
            .unwrap();
        } else {
            writeln!(out, "{id:?} {start:>4}..{end:<4} {:?}", tok.kind).unwrap();
        }
        if matches!(tok.kind, Ident | RawIdent) && !idents.iter().any(|&s| s == slice) {
            idents.push(tokens[id]);
        }
        offset += tok.len;
    }

    writeln!(
        out,
        "\ntokens (IndexVec<TokenId, _>): {}",
        tokens.len()
    )
    .unwrap();
    writeln!(
        out,
        "unique idents (DroplessArena, {} bytes): {}",
        idents.iter().map(|s| s.len()).sum::<usize>(),
        idents.join(", ")
    )
    .unwrap();
    out
}
