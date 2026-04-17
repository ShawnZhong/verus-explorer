// RA-powered front end: take user-supplied Rust source and run it through
// rust-analyzer's parse + name-resolution + type-inference pipeline (as
// exposed by `hir::Semantics`) to recover inferred types.
//
// This is the structural analogue of upstream Verus's `rust_to_vir` —
// minus rustc — and forms the source-end of the pipeline documented in
// `docs/roadmap.md § architecture at target`. For now it only surfaces
// inferred types; translation to `vir::ast::Krate` is the next step.
//
// The `test-fixture` dep is used for its `RootDatabase::with_single_file`
// helper; that's the same API rust-analyzer's own tests use to set up an
// in-memory DB from a source string.

use hir::{HirDisplay, Semantics};
use ide_db::RootDatabase;
use syntax::{AstNode, ast};
use test_fixture::WithFixture;

/// Infer the type of the tail expression of the first `fn` in `source`.
/// Returns the displayed type, or `None` if parsing found no function
/// with a tail expression.
pub fn infer_first_fn_tail(source: &str) -> Option<String> {
    let (db, file_id) = RootDatabase::with_single_file(source);
    let sema = Semantics::new(&db);
    let tree = sema.parse(file_id);

    let func_node = tree.syntax().descendants().find_map(ast::Fn::cast)?;
    let body = func_node.body()?;
    let tail = body.tail_expr()?;
    let info = sema.type_of_expr(&tail)?;

    // hir::Type::display wants a DisplayTarget, built from the function's
    // owning Crate. Semantics::to_def on an ast::Fn yields a hir::Function;
    // from there we walk to the module -> crate.
    let func_def = sema.to_def(&func_node)?;
    let krate = func_def.module(&db).krate(&db);
    let dt = hir::DisplayTarget::from_crate(&db, krate.into());
    Some(format!("{}", info.original().display(&db, dt)))
}

