//! Real-binary PTY smoke tests for the /worktrees pane. #[ignore] (each
//! spawns the houyi binary + a PTY -- too slow for the 60s commit gate).
//! Run via make test ui (builds the bin first) or
//! cargo test --test ui_worktree -- --ignored after cargo build --bin houyi.
//!
//! Industrial-usability proof for the worktree surface: launch the real
//! binary in the workspace root (a real git repo with linked worktrees),
//! drive /worktree through a real terminal, and assert the pane renders the
//! worktree list. The inline unit layer (parse_worktrees + cursor tests)
//! proves the data path; this layer proves a user can actually open the
//! pane and see their worktrees. The worktree-list surface has no
//! equivalent command elsewhere.

#![allow(clippy::unwrap_in_result)]

mod common;

use common::{
    Key, RENDER_TIMEOUT, run_slash_command, session_on_working, session_on_working_in_repo,
};
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

/// /worktree opens the pane and renders the list header plus at least one
/// worktree row (the main worktree is always present in a git repo). The
/// workspace root is a real git repo, so the pane lists real worktrees.
#[test]
#[ignore]
fn test_pane_lists_worktrees() {
    let mut s = session_on_working();
    run_slash_command(&mut s, "worktree");
    assert!(
        s.wait_for("worktrees —", RENDER_TIMEOUT),
        "worktrees pane header should render:\n{}",
        s.output()
    );
    // The main worktree (the repo root) is always listed; its branch is the
    // current dev/main branch. Assert a branch bracket renders so the test
    // does not depend on the exact path (which varies by checkout).
    assert!(
        s.output().contains('['),
        "at least one worktree row with a branch bracket should render:\n{}",
        s.output()
    );
    assert!(
        !s.output().contains("no project root"),
        "running in a git repo, the no-root fallback must not fire:\n{}",
        s.output()
    );
    drop(s);
}

/// Typing into the /worktrees pane filters the list (the search feature).
/// A char pushes into the search query, the search hint line renders, and
/// the first Esc clears the query (not the pane) so a typo does not dismiss
/// the whole pane. This is the path-truncation + search surface.
#[test]
#[ignore]
fn test_pane_search_esc_clears() {
    let mut s = session_on_working();
    run_slash_command(&mut s, "worktree");
    // Wait for the pane to render before typing into its search.
    assert!(
        s.wait_for("worktrees —", RENDER_TIMEOUT),
        "worktrees pane header should render:\n{}",
        s.output()
    );
    s.send_key(&Key::Char('x'));
    assert!(
        s.wait_for("search:", RENDER_TIMEOUT),
        "the search hint line should render after typing:\n{}",
        s.output()
    );
    // First Esc clears the search query (the hint disappears), not the pane.
    // The pane stays open (header still rendered).
    s.send_key(&Key::Esc);
    assert!(
        s.wait_for("worktrees —", RENDER_TIMEOUT),
        "Esc should clear the search, not close the pane:\n{}",
        s.output()
    );
    drop(s);
}

/// Seed a throwaway git repo the binary can run in: a workspace manifest so
/// resolve_project_workspace pins the dir, plus one empty commit so branching
/// from HEAD succeeds. enter_worktree then creates a real linked worktree
/// under the repo state dir, never in the developer real workspace.
#[allow(clippy::disallowed_methods)]
fn make_temp_repo(slug: u64) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("houyi-u8-repo-{}-{slug}", std::process::id()));
    drop(std::fs::remove_dir_all(&dir));
    std::fs::create_dir_all(&dir).expect("mkdir repo");
    std::fs::write(dir.join("Cargo.toml"), "[workspace]\nmembers = []\n").expect("write manifest");
    for args in [
        &["init", "-q"][..],
        &["config", "user.email", "t@x"][..],
        &["config", "user.name", "t"][..],
        &["commit", "--allow-empty", "-m", "init", "-q"][..],
    ] {
        let ok = Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(args)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "git {:?}", args);
    }
    dir
}

/// A three-response script: enter a worktree, then exit with remove + discard,
/// then end the turn. enter auto-runs in Auto mode; exit(remove) must raise
/// the approval card (the inner tool declares remove needs approval, forwarded
/// by the guarded wrapper) so the worktree is not deleted until the human
/// confirms.
const EXIT_REMOVE_ASK_SCRIPT: &str = r#"[
  [{"type":"ToolCall","id":"c1","name":"enter_worktree","input":{"name":"u8"}}],
  [{"type":"ToolCall","id":"c2","name":"exit_worktree","input":{"action":"remove","discard_changes":true}}],
  [{"type":"Text","text":"done"}]
]"#;

/// The approval card must fire for exit_worktree(remove) in Auto mode (the
/// guarded wrapper forwards the inner tool per-input approval signal), and
/// the worktree directory must survive unconfirmed + be deleted only after
/// the human confirms. Pins the runtime Ask -> confirm -> resume -> remove
/// chain through the real binary + real sandbox + real git worktree; the unit
/// layer covers the controller delete + the gate decide in isolation, not the
/// cross-layer resume path. Worktree created + removed under a throwaway repo
/// so the developer workspace is never touched.
#[test]
#[ignore]
fn test_exit_remove_asks_deletes() {
    let repo = make_temp_repo(1);
    let wt_dir = repo.join(".houyicoder").join("worktrees").join("u8");
    let mut s = session_on_working_in_repo(repo.clone(), EXIT_REMOVE_ASK_SCRIPT);
    // Kick the run: a plain message + Enter ships the user turn.
    s.send_str("go");
    s.send_key(&Key::Enter);
    // enter_worktree runs in Auto mode (auto-allowed), then the runner asks the
    // provider again + gets the exit_worktree remove ToolCall. The approval
    // card header appearing is the signal enter already ran (the loop is
    // sequential: exit only dispatches after enter result lands). Seatbelt
    // narrow + git worktree add take a few seconds, so the wait is generous.
    let card_timeout = Duration::from_secs(15);
    assert!(
        s.wait_for("Exit_worktree command", card_timeout),
        "the approval card header should render for exit_worktree remove \
         (enter must have run first):\n{}",
        s.output()
    );
    // enter ran: the worktree directory exists on disk.
    assert!(
        wt_dir.exists(),
        "the worktree directory should exist after enter ran:\n{}",
        s.output()
    );
    // The card shows the remove action in the args line + the Yes option.
    assert!(
        s.output_plain().contains("remove"),
        "the card should show the remove action in the args line:\n{}",
        s.output()
    );
    assert!(
        s.wait_for("1. Yes", RENDER_TIMEOUT),
        "the Yes option should render on the approval card:\n{}",
        s.output()
    );
    // Unconfirmed: the worktree dir must survive (the human has not answered).
    assert!(
        wt_dir.exists(),
        "the worktree directory must survive while the card is unconfirmed:\n{}",
        s.output()
    );
    // Confirm Yes (default focus) -> resume -> execute_authorized -> remove.
    s.send_key(&Key::Enter);
    // The remove deletes the worktree dir + branch; the run ends on "done".
    let remove_timeout = Duration::from_secs(15);
    assert!(
        s.wait_for("done", remove_timeout),
        "confirming should resume the run, remove the worktree, and end:\n{}",
        s.output()
    );
    assert!(
        !wt_dir.exists(),
        "the worktree directory should be gone after a confirmed remove:\n{}",
        s.output()
    );
    drop(s);
    std::fs::remove_dir_all(&repo).ok();
}

/// A four-response script: enter a worktree, dirty it with an untracked file
/// via bash (touch is safe -- no redirect, no deletion word, so Auto allows
/// it without a mid-flow card), then exit with remove and discard_changes
/// left at its false default, then end the turn. The remove still raises the
/// approval card (approval is action-based, not state-based); confirming
/// runs the controller exit, which must refuse because the worktree has
/// uncommitted work and discard_changes is false.
/// The closing text is deliberately not "done": the touch above is a
/// silent-success command, whose result row renders the label "done", so
/// "done" cannot serve as an end-of-run marker in this script.
const EXIT_REMOVE_REFUSE_SCRIPT: &str = r#"[
  [{"type":"ToolCall","id":"c1","name":"enter_worktree","input":{"name":"u8d"}}],
  [{"type":"ToolCall","id":"c2","name":"bash","input":{"command":"touch wip.txt"}}],
  [{"type":"ToolCall","id":"c3","name":"exit_worktree","input":{"action":"remove"}}],
  [{"type":"Text","text":"turn wrapped up"}]
]"#;

/// The fail-closed discard gate: exit_worktree(remove) with discard_changes
/// false (the default) on a worktree that has uncommitted work must refuse
/// even after the human confirms the remove card. The remove still asks
/// (approval is keyed on the action, not the discard flag), so the card fires
/// first; confirming resumes onto the controller exit, which returns the
/// refuse error listing the work + pointing the model back to the user. The
/// worktree directory must survive (not deleted) and the run must still end.
/// Without this path the approval card would be a rubber stamp -- the
/// discard gate is the second of the two gates on a remove.
#[test]
#[ignore]
fn test_remove_refuses_uncommitted_work() {
    let repo = make_temp_repo(2);
    let wt_dir = repo.join(".houyicoder").join("worktrees").join("u8d");
    let mut s = session_on_working_in_repo(repo.clone(), EXIT_REMOVE_REFUSE_SCRIPT);
    s.send_str("go");
    s.send_key(&Key::Enter);
    // enter runs, then bash touch dirties the worktree, then exit(remove)
    // raises the approval card. The card appearing means enter + touch ran.
    let card_timeout = Duration::from_secs(15);
    assert!(
        s.wait_for("Exit_worktree command", card_timeout),
        "the approval card header should render for exit_worktree remove:\n{}",
        s.output()
    );
    assert!(
        wt_dir.exists(),
        "the worktree directory should exist after enter + touch:\n{}",
        s.output()
    );
    assert!(
        s.wait_for("1. Yes", RENDER_TIMEOUT),
        "the Yes option should render on the approval card:\n{}",
        s.output()
    );
    // Confirm Yes -> resume -> controller exit(Remove, discard=false). The
    // worktree has 1 uncommitted file -> the discard gate refuses.
    s.send_key(&Key::Enter);
    let end_timeout = Duration::from_secs(15);
    // Wait on the refuse text, not on a generic end-of-run word: the result
    // row is what proves the user is told, and it is also the signal that the
    // gate has run. The row body is previewed (truncated to the row width) in
    // the live view, so the assertion targets the head of the message; the
    // full three-part wording is pinned where it is not width-dependent, in
    // the controller test that owns the message.
    assert!(
        s.wait_for_plain("1 uncommitted file", end_timeout),
        "the refuse error should render in the exit_worktree result row:\n{}",
        s.output()
    );
    // Refused -> the worktree dir survives (no deletion).
    assert!(
        wt_dir.exists(),
        "the worktree directory must survive a refused remove:\n{}",
        s.output()
    );
    drop(s);
    std::fs::remove_dir_all(&repo).ok();
}
