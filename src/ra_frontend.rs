// Minimal rust-analyzer bridge: take a single Rust source file, create a
// one-file in-memory crate, and lower the first parsed function into HIR.

use base_db::{
    CrateGraphBuilder, CrateOrigin, CrateWorkspaceData, EditionedFileId, Env, FileSet, SourceRoot,
    VfsPath,
};
use hir::Semantics;
use hir_expand::change::ChangeWithProcMacros;
use ide_db::RootDatabase;
use paths::AbsPathBuf;
use span::{Edition, FileId};
use syntax::{AstNode, ast};
use triomphe::Arc;

const ROOT_FILE_ID: FileId = FileId::from_raw(0);
const ROOT_FILE_PATH: &str = "/main.rs";
const PROC_MACRO_CWD: &str = "/workspace";

/// Build a fresh `RootDatabase` containing exactly one file whose text is
/// `source`, wrapped in a single local crate.
fn setup_single_file_db(source: &str) -> (RootDatabase, EditionedFileId) {
    let mut db = RootDatabase::new(None);

    let mut file_set = FileSet::default();
    file_set.insert(
        ROOT_FILE_ID,
        VfsPath::new_virtual_path(ROOT_FILE_PATH.to_string()),
    );

    let mut crate_graph = CrateGraphBuilder::default();
    crate_graph.add_crate_root(
        ROOT_FILE_ID,
        Edition::CURRENT,
        /* display_name */ None,
        /* version */ None,
        Default::default(),
        /* potential_cfg_options */ None,
        Env::default(),
        CrateOrigin::Local {
            repo: None,
            name: None,
        },
        /* crate_attrs */ Vec::new(),
        /* is_proc_macro */ false,
        Arc::new(AbsPathBuf::assert(PROC_MACRO_CWD.into())),
        Arc::new(CrateWorkspaceData {
            target: Err("fixture has no layout".into()),
            toolchain: None,
        }),
    );

    let mut change = ChangeWithProcMacros::default();
    change.set_roots(vec![SourceRoot::new_local(file_set)]);
    change.change_file(ROOT_FILE_ID, Some(source.to_string()));
    change.set_crate_graph(crate_graph);
    change.apply(&mut db);

    let file_id = EditionedFileId::current_edition(&db, ROOT_FILE_ID);
    (db, file_id)
}

/// Return a small HIR summary for the first `fn` in `source`.
pub fn first_fn_hir(source: &str) -> Option<String> {
    let (db, file_id) = setup_single_file_db(source);
    let sema = Semantics::new(&db);
    let tree = sema.parse(file_id);

    let func_node = tree.syntax().descendants().find_map(ast::Fn::cast)?;
    let func = sema.to_def(&func_node)?;
    Some(format!(
        "fn {}",
        func.name(&db).display(&db, Edition::CURRENT)
    ))
}
