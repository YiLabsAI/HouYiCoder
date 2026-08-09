//! Real-binary PTY tests for the run-time approval-card flow. The card fires
//! when a guarded tool is ASKED (Manual mode, or a destructive command under
//! Auto). The stub provider used to return only plain text, so the card was
//! never PTY-reachable; with FakeProvider driving a real ToolCall the card
//! fires end-to-end. The user's #1 concern: after answering (approve/deny),
//! the transcript must not leave a stale error line.
//!
//! Run via make test ui (builds the bin first) or
//! cargo test --test ui_approval -- --ignored after cargo build --bin houyi.

#![allow(clippy::unwrap_in_result)]

mod common;

use common::{Key, RENDER_TIMEOUT, session_on_working_in_repo};
use std::path::PathBuf;
use std::process::Command;

/// A two-response script: the first carries a bash ToolCall so the runner
/// raises a permission ask (in Manual mode bash is guarded -> ASK; read-only
/// tools like read/glob auto-run even in Manual, so a mutating tool is needed
/// to fire the card). The echo hi command is non-destructive + sandbox-allowed, so it
/// succeeds after the approve — no stale error. The second response ends the
/// run cleanly.
const BASH_ASK_SCRIPT: &str = r#"[
  [{"type":"ToolCall","id":"c1","name":"bash","input":{"command":"echo hi"}}],
  [{"type":"Text","text":"done"}]
]"#;

/// Seed a throwaway git repo the binary can run in (a workspace manifest so
/// resolve_project_workspace pins the dir, one commit so branching from HEAD
/// succeeds). PTY tests should run in an isolated repo, not the developer
/// workspace root -- the root's project memory / settings / session state can
/// delay stub delivery past the render timeout.
#[allow(clippy::disallowed_methods)]
fn make_temp_repo(slug: u64) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("houyi-approval-repo-{}-{slug}", std::process::id()));
    drop(std::fs::remove_dir_all(&dir));
    std::fs::create_dir_all(&dir).expect("mkdir repo");
    std::fs::write(dir.join("Cargo.toml"), "[workspace]\nmembers = []\n").expect("write manifest");
    for args in [
        &["init", "-q"][..],
        &["config", "user.email", "t@x"][..],
        &["config", "user.name", "t"][..],
        &["add", "Cargo.toml"][..],
        &["commit", "-m", "init", "-q"][..],
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

/// #37: in Manual mode a guarded bash raises the approval card; approving it
/// resumes the run, the command runs + succeeds, and the transcript ends clean
/// — no stale "error:" line. Pins the whole ask -> answer -> resume -> render
/// chain through the real binary (the unit layer covers only the decide logic;
/// read-only tools auto-run even in Manual, so bash is the ASK trigger).
#[test]
#[ignore]
fn test_approve_bash_no_error() {
    let mut s = session_on_working_in_repo(make_temp_repo(1), BASH_ASK_SCRIPT);
    // Local mode starts in Auto (auto-approve); cycle to Manual so guarded
    // tools ASK instead of auto-running.
    s.send_key(&Key::Backtab);
    assert!(
        s.wait_for("manual mode on", RENDER_TIMEOUT),
        "shift+tab should cycle to manual (ask) mode:\n{}",
        s.output()
    );
    s.send_str("run it");
    s.send_key(&Key::Enter);
    // The run hits the bash ToolCall -> permission ask -> the inline approval
    // card renders with the Yes / Yes-don't-ask / No options.
    assert!(
        s.wait_for("1. Yes", RENDER_TIMEOUT),
        "the approval card should render for a guarded bash in manual mode:\n{}",
        s.output()
    );
    // Default focus is Yes (index 0); Enter confirms the verdict + ships it.
    s.send_key(&Key::Enter);
    // The verdict resumes the run: bash runs (echo hi), the second response
    // ("done") ends the turn.
    assert!(
        s.wait_for("done", RENDER_TIMEOUT),
        "approving should resume the run + render the done reply:\n{}",
        s.output()
    );
    // The user's #1 invariant: no stale error line after the verdict.
    assert!(
        !s.output().contains("error:"),
        "the transcript must not leave a stale error after approving:\n{}",
        s.output()
    );
}

/// The deny half of the flow: rejecting the bash (Esc) clears the card, the
/// run still ends, and no approval card is left stranded on screen. Catches a
/// deny that leaves a stale "1. Yes" card or strands agent_busy.
#[test]
#[ignore]
fn test_deny_bash_clears_ends() {
    let mut s = session_on_working_in_repo(make_temp_repo(2), BASH_ASK_SCRIPT);
    s.send_key(&Key::Backtab);
    assert!(
        s.wait_for("manual mode on", RENDER_TIMEOUT),
        "shift+tab should cycle to manual mode:\n{}",
        s.output()
    );
    s.send_str("run it");
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for("1. Yes", RENDER_TIMEOUT),
        "the approval card should render:\n{}",
        s.output()
    );
    // Esc rejects the current approval (handle_approval Esc arm). The bare
    // 0x1b needs a gap so crossterm resolves it as Esc, not a sequence start.
    s.send_key(&Key::Esc);
    std::thread::sleep(std::time::Duration::from_millis(200));
    // The reject verdict ships; the run resumes + ends on "done".
    assert!(
        s.wait_for("done", RENDER_TIMEOUT),
        "denying should still let the run end:\n{}",
        s.output()
    );
    // Wipe history so the absence check reads the CURRENT render: the card
    // must be gone (no stranded "1. Yes") after the run ended.
    s.clear_output();
    s.wait_for("auto mode on", RENDER_TIMEOUT);
    assert!(
        !s.output().contains("1. Yes"),
        "the approval card should clear after denying (no stranded card):\n{}",
        s.output()
    );
}
