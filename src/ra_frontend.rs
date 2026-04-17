// RA-powered front end: take user-supplied Rust source and run it through
// rust-analyzer's parse + name-resolution + type-inference pipeline (as
// exposed by `hir::Semantics`) to recover inferred types.
//
// This is the structural analogue of upstream Verus's `rust_to_vir` —
// minus rustc — and forms the source-end of the pipeline documented in
// `docs/roadmap.md § architecture at target`. For now it only surfaces
// inferred types; translation to `vir::ast::Krate` is the next step.
//
// The RootDatabase setup below is a hand-rolled minimum: one file, one
// crate, no stdlib, no proc-macros. It deliberately avoids `test-fixture`
// — that crate is test-only and calls `std::env::current_dir()`, which
// returns `Err(Unsupported)` on `wasm32-unknown-unknown` and panics at
// bootstrap time. The cost of going direct is ~50 extra lines of wiring
// against base-db / hir-expand; the win is a browser-safe setup path.

use std::path::PathBuf;

use base_db::{
    CrateGraphBuilder, CrateOrigin, CrateWorkspaceData, EditionedFileId, Env, FileSet, SourceRoot,
    VfsPath,
    target::{Arch, TargetData},
};
use cfg::CfgOptions;
use hir::{HirDisplay, Semantics};
use hir_expand::change::ChangeWithProcMacros;
use ide_db::RootDatabase;
use paths::AbsPathBuf;
use span::{Edition, FileId};
use syntax::{AstNode, ast};
use triomphe::Arc;

const ROOT_FILE_ID: FileId = FileId::from_raw(0);

// x86_64-unknown-linux-gnu data layout — used only by the tiny fraction
// of hir-ty queries that need numeric layout info. For simple arithmetic
// / comparison / tuple inference none of them fire.
const DUMMY_X86_64_LAYOUT: &str = "e-m:e-p270:32:32-p271:32:32-p272:64:64-i64:64-i128:128-f80:128-n8:16:32:64-S128";

/// Build a fresh `RootDatabase` containing exactly one file whose text is
/// `source`, wrapped in a single local crate. Mirrors what `test-fixture`
/// does for a plain no-metadata single-file fixture, minus the fixture-
/// parsing, proc-macro scaffolding, and `current_dir()` call.
fn setup_single_file_db(source: &str) -> (RootDatabase, EditionedFileId) {
    let mut db = RootDatabase::new(None);

    let mut file_set = FileSet::default();
    file_set.insert(ROOT_FILE_ID, VfsPath::new_virtual_path("/main.rs".to_string()));
    let source_root = SourceRoot::new_local(file_set);

    let ws_data = Arc::new(CrateWorkspaceData {
        target: Ok(TargetData { arch: Arch::Other, data_layout: DUMMY_X86_64_LAYOUT.into() }),
        toolchain: None,
    });

    // `proc_macro_cwd` must be absolute — hir-def stores it but never derefs
    // for crates with no proc-macro deps, so "/" is a safe synthetic value
    // on hosts where `current_dir()` isn't available (e.g. wasm32).
    let proc_macro_cwd = Arc::new(AbsPathBuf::assert_utf8(PathBuf::from("/")));

    let mut crate_graph = CrateGraphBuilder::default();
    crate_graph.add_crate_root(
        ROOT_FILE_ID,
        Edition::Edition2024,
        /* display_name */ None,
        /* version */ None,
        CfgOptions::default(),
        /* potential_cfg_options */ None,
        Env::default(),
        CrateOrigin::Local { repo: None, name: None },
        /* crate_attrs */ Vec::new(),
        /* is_proc_macro */ false,
        proc_macro_cwd,
        ws_data,
    );

    let mut change = ChangeWithProcMacros::default();
    change.set_roots(vec![source_root]);
    change.change_file(ROOT_FILE_ID, Some(source.to_string()));
    change.set_crate_graph(crate_graph);
    change.apply(&mut db);

    let efid = EditionedFileId::new(&db, ROOT_FILE_ID, Edition::Edition2024);
    (db, efid)
}

/// Infer the type of the tail expression of the first `fn` in `source`.
/// Returns the displayed type, or `None` if no function with a tail
/// expression was found.
pub fn infer_first_fn_tail(source: &str) -> Option<String> {
    let (db, file_id) = setup_single_file_db(source);
    let sema = Semantics::new(&db);
    let tree = sema.parse(file_id);

    let func_node = tree.syntax().descendants().find_map(ast::Fn::cast)?;
    let body = func_node.body()?;
    let tail = body.tail_expr()?;
    let info = sema.type_of_expr(&tail)?;

    let func_def = sema.to_def(&func_node)?;
    let krate = func_def.module(&db).krate(&db);
    let dt = hir::DisplayTarget::from_crate(&db, krate.into());
    Some(format!("{}", info.original().display(&db, dt)))
}
