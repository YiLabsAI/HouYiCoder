//! What a reclaim reports when it cannot finish, and the panic text it logs.

use std::path::PathBuf;

use super::super::ChildCleanup;
use super::*;

fn sample_record() -> WorktreeReclaimRecord {
    WorktreeReclaimRecord {
        repo_root: PathBuf::from("/repo"),
        worktree_path: PathBuf::from("/repo/.houyicoder/worktrees/swept"),
        worktree_branch: "worktree-swept".to_string(),
        slug: "swept".to_string(),
        head_commit: Some("0".repeat(40)),
    }
}

/// A reclaim that panics keeps its tree: the tree was never proven clean, so
/// reporting a removal would claim a directory that may still hold work is
/// gone.
#[test]
fn test_panic_keeps_tree() {
    let record = sample_record();
    let outcome = reclaim_panic_outcome(&record, &"boom");

    match outcome {
        ChildCleanup::Kept {
            worktree_path,
            worktree_branch,
        } => {
            assert_eq!(worktree_path, record.worktree_path);
            assert_eq!(worktree_branch, record.worktree_branch);
        }
        other => panic!("a panicking reclaim has to keep the tree: {other:?}"),
    }
}

/// The log line says what the panic said, whichever of the two forms a panic
/// payload takes.
#[test]
fn test_panic_text_reads_payload() {
    assert_eq!(panic_text(&"boom"), "boom");
    assert_eq!(panic_text(&String::from("boom")), "boom");
    assert_eq!(panic_text(&7u32), "no message");
}
