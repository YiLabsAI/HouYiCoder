//! Live fence test for the worktree WRITE-isolation property (journey H-1,
//! revised). The narrow worktree fence refuses a WRITE of the main tree
//! (outside writable_roots) while allowing a write inside the worktree
//! (positive control, else the refuse is vacuous). Reads of the main tree
//! are open on purpose: a linked worktree is a checkout of the same repo, so
//! reading the main tree is reading the same repo's main branch, not a
//! cross-boundary leak, and a worktree often needs to look back at the main
//! tree (branch context). Read-isolation is too strict without buying
//! safety, so this fence isolates writes only (the genuine danger: a
//! worktree agent corrupting the main tree). Runs real sandbox-exec;
//! ignored (too slow for the commit gate).

#![allow(clippy::disallowed_methods)]

use super::super::temp_dir::COUNTER;
use super::super::*;
use houyicoder_api::sandbox::SandboxSession;
use std::env::temp_dir;
use std::fs::create_dir_all;
use std::fs::read_to_string;
use std::fs::remove_dir_all;
use std::fs::write;
use std::process::Command;
use std::process::id;
use std::sync::atomic::Ordering;

/// resolve admits the per-session tmp dir (allow-listed + exported as
/// TMPDIR) so the Write and Edit tools can write temp files; a literal /tmp
/// path outside this session's subdir stays rejected.
#[test]
fn test_resolve_admits_session_tmpdir() {
    let s = MacSeatbeltSession::new().unwrap();
    let tmp_file = s.tmpdir.join("analyze.py");
    assert!(
        s.resolve(tmp_file.to_str().unwrap()).is_ok(),
        "resolve must admit the per-session tmp dir"
    );
    assert!(matches!(
        s.resolve("/tmp/analyze.py").err().unwrap(),
        SandboxError::PathTraversal(_)
    ));
}

/// Narrowing a session to a linked worktree keeps the main checkout readable
/// for Read/Grep/Glob while refusing writes to the main checkout.
#[tokio::test]
async fn test_narrow_worktree_reads_main() {
    let parent = temp_dir().join(format!(
        "houyi-narrow-read-{}-{}",
        id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    create_dir_all(&parent).expect("mkdir parent");
    let repo = parent.join("repo");
    create_dir_all(&repo).expect("mkdir repo");
    let run = |args: &[&str]| Command::new("git").arg("-C").arg(&repo).args(args).status();
    assert!(run(&["init", "-q"]).unwrap().success());
    let _ = run(&["config", "user.email", "t@x"]).unwrap();
    let _ = run(&["config", "user.name", "t"]).unwrap();
    write(repo.join("README.md"), "main-readme\n").expect("write readme");
    assert!(run(&["add", "README.md"]).unwrap().success());
    assert!(run(&["commit", "-m", "init", "-q"]).unwrap().success());
    let wt = repo.join(".houyicoder/worktrees/narrow-read");
    create_dir_all(wt.parent().unwrap()).expect("mkdir wt parent");
    assert!(
        run(&["worktree", "add", &wt.to_string_lossy(), "-q"])
            .unwrap()
            .success()
    );

    let s = MacSeatbeltSession::new_in_cwd(&repo).expect("session");
    let guard = s
        .narrow_to_worktree(&wt, &repo.join(".git"))
        .expect("narrow");
    let main_readme = repo.join("README.md").canonicalize().expect("canonical");
    let main_str = main_readme.to_string_lossy();
    let bytes = s
        .read_file(&main_str, 64)
        .await
        .expect("read main checkout from worktree");
    assert_eq!(bytes, b"main-readme\n");
    assert!(
        s.resolve_write(&main_str).is_err(),
        "writes to the main checkout must stay refused while narrowed"
    );
    guard.restore().expect("restore");
    let _cleanup = remove_dir_all(&parent);
}

/// The narrow worktree fence refuses a write of the main tree and allows a
/// write inside the worktree. Pins the genuine isolation (writes), not reads.
#[tokio::test]
#[ignore]
async fn test_worktree_refuses_main_write() {
    let parent = temp_dir().join(format!(
        "houyi-narrow-write-{}-{}",
        id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    create_dir_all(&parent).expect("mkdir parent");
    let repo = parent.join("repo");
    create_dir_all(&repo).expect("mkdir repo");
    let run = |args: &[&str]| Command::new("git").arg("-C").arg(&repo).args(args).status();
    assert!(run(&["init", "-q"]).unwrap().success(), "git init");
    let _ = run(&["config", "user.email", "t@x"]).unwrap();
    let _ = run(&["config", "user.name", "t"]).unwrap();
    write(repo.join("Cargo.toml"), "[workspace]\nmembers = []\n").expect("write manifest");
    assert!(run(&["add", "Cargo.toml"]).unwrap().success(), "git add");
    assert!(
        run(&["commit", "-m", "init", "-q"]).unwrap().success(),
        "init commit"
    );
    // A main-tree file the worktree must NOT be able to write (outside
    // writable_roots). Committed so it exists in both the main tree and the
    // linked worktree checkout; the write targets the MAIN-tree path.
    write(repo.join("main_only.txt"), "orig\n").expect("write main file");
    let wt = repo
        .join(".houyicoder")
        .join("worktrees")
        .join("narrow-write");
    create_dir_all(wt.parent().unwrap()).expect("mkdir worktree parent");
    assert!(
        run(&["worktree", "add", &wt.to_string_lossy(), "-q"])
            .unwrap()
            .success(),
        "worktree add"
    );
    let s = MacSeatbeltSession::new_in_cwd(&repo).expect("session");
    let guard = s
        .narrow_to_worktree(&wt, &repo.join(".git"))
        .expect("narrow");

    // Write to the main-tree file by absolute path: outside the worktree's
    // writable_roots, so the fence must refuse. The original content survives.
    let main_file = repo.join("main_only.txt");
    let refused = s
        .exec(&format!("echo appended >> {}", main_file.display()))
        .await
        .expect("exec");
    assert!(
        !refused.is_success(),
        "a write to the main tree must be refused by the narrow worktree fence: {:?}",
        refused
    );
    assert_eq!(
        read_to_string(&main_file).unwrap(),
        "orig\n",
        "the main-tree file must be unchanged after a refused write"
    );

    // Positive control: a write INSIDE the worktree succeeds, so the refuse
    // was the fence refusing the main-tree path, not the write path being
    // broken for any other reason (paired allow with the deny).
    let inside = s
        .exec("echo wt-edit >> Cargo.toml")
        .await
        .expect("exec inside");
    assert!(
        inside.is_success(),
        "a write inside the worktree must succeed (positive control): {:?}",
        inside
    );
    assert!(
        read_to_string(wt.join("Cargo.toml"))
            .unwrap()
            .contains("wt-edit"),
        "the worktree file should reflect the inside write"
    );

    guard.restore().expect("restore");
    drop(s);
    remove_dir_all(&parent).ok();
}
