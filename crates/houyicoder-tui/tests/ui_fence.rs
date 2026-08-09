//! Real-binary PTY tests for fence isolation across the worktree/egress
//! surface. Three behaviours a unit test cannot prove end-to-end:
//! - a worktree's narrow fence refuses a WRITE of the main tree (the genuine
//!   isolation; reads of the main tree are open on purpose — a worktree is a
//!   checkout of the same repo, so reading the main tree is reading the same
//!   repo's main branch, not a leak, and a worktree agent does not
//!   read-isolate its worktree either);
//! - under the default-open network posture, a bash egress command still
//!   raises the per-command approval card (the gate's egress ask is immune
//!   to mode, so the open default does not auto-allow egress);
//! - inside a worktree, an egress command still raises the card (the narrow
//!   fence does not silence the gate).
//!
//! Run via make test ui, or cargo test --test ui_fence -- --ignored.

#![allow(clippy::unwrap_in_result)]

mod common;

use common::{Key, session_on_working, session_on_working_in_repo};
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

/// Seed a throwaway git repo the binary can run in: a workspace manifest so
/// resolve_project_workspace pins the dir, one commit so branching from HEAD
/// succeeds, and a main-tree file the worktree must NOT be able to write.
#[allow(clippy::disallowed_methods)]
fn make_temp_repo(slug: u64) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("houyi-fence-repo-{}-{slug}", std::process::id()));
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
    // A main-tree file the worktree must not write (outside writable_roots).
    std::fs::write(dir.join("main_only.txt"), "orig\n").expect("write main file");
    dir
}

/// Enter a worktree, then bash append to the main-tree file by absolute path.
/// The append is destructive so the gate asks first; confirming runs the
/// command, which the narrow worktree fence refuses (the main tree is
/// outside writable_roots), so the main-tree file stays unchanged. The
/// live seatbelt test is the authoritative write-isolation proof; this PTY
/// layer proves the gate-ask + fence-refuse chain through the real binary.
#[test]
#[ignore]
fn test_worktree_refuses_tree_write() {
    let repo = make_temp_repo(1);
    let main_file = repo.join("main_only.txt");
    let script = format!(
        r#"[
  [{{"type":"ToolCall","id":"c1","name":"enter_worktree","input":{{"name":"h1w"}}}}],
  [{{"type":"ToolCall","id":"c2","name":"bash","input":{{"command":"echo leaked >> {main_file}"}}}}],
  [{{"type":"Text","text":"done"}}]
]"#,
        main_file = main_file.display(),
    );
    let mut s = session_on_working_in_repo(repo.clone(), &script);
    s.send_str("go");
    s.send_key(&Key::Enter);
    // enter runs, then the destructive append raises the approval card.
    let card = Duration::from_secs(20);
    assert!(
        s.wait_for("1. Yes", card),
        "the destructive-append approval card should render:\n{}",
        s.output()
    );
    // Confirm Yes (default focus) -> the command runs -> the narrow worktree
    // fence refuses the write to the main tree -> bash fails -> the model gets
    // the final "done" response.
    s.send_key(&Key::Enter);
    let end = Duration::from_secs(15);
    assert!(
        s.wait_for("done", end),
        "confirming should resume, the fence refuse feeds back, the run ends:\n{}",
        s.output()
    );
    // The main-tree file is unchanged: the narrow worktree fence refused the
    // write even after the user confirmed. If this fails, the worktree is not
    // write-isolated (the worktree agent can corrupt the main tree).
    assert_eq!(
        std::fs::read_to_string(&main_file).unwrap(),
        "orig\n",
        "the main-tree file must be unchanged after a confirmed-but-fence-refused write:\n{}",
        s.output()
    );
    drop(s);
    std::fs::remove_dir_all(&repo).ok();
}

/// Under the default-open network posture, a bash egress command still raises
/// the per-command approval card (the gate's egress ask is immune to mode, so
/// the open default does not auto-allow egress — the card, not the kernel, is
/// the control). The card fires before exec, so no real network call is made.
#[test]
#[ignore]
fn test_default_open_asks_egress() {
    let script = r#"[
  [{"type":"ToolCall","id":"c1","name":"bash","input":{"command":"curl example.com"}}],
  [{"type":"Text","text":"done"}]
]"#;
    let mut s = session_on_working_in_repo(make_temp_repo(3), script);
    s.send_str("go");
    s.send_key(&Key::Enter);
    // The egress card fires (gate asks) before curl runs. The command text
    // appears on the card, and the Yes option renders.
    let card = Duration::from_secs(10);
    assert!(
        s.wait_for("1. Yes", card),
        "the egress approval card should render for curl under the default-open posture:\n{}",
        s.output()
    );
    assert!(
        s.output_plain().contains("curl"),
        "the card should show the curl command:\n{}",
        s.output()
    );
    drop(s);
}

/// Inside a worktree (narrow fence), a bash egress command still raises the
/// per-command approval card — the narrow fence does not silence the gate's
/// egress ask. The card fires before exec; the fence would refuse at exec
/// time (the narrow profile carries a network deny), but that is the
/// mechanism-tested part — this layer proves the ask reaches the user.
#[test]
#[ignore]
fn test_worktree_asks_egress() {
    let repo = make_temp_repo(2);
    let script = r#"[
  [{"type":"ToolCall","id":"c1","name":"enter_worktree","input":{"name":"x1"}}],
  [{"type":"ToolCall","id":"c2","name":"bash","input":{"command":"curl example.com"}}],
  [{"type":"Text","text":"done"}]
]"#;
    let mut s = session_on_working_in_repo(repo.clone(), script);
    s.send_str("go");
    s.send_key(&Key::Enter);
    let card = Duration::from_secs(20);
    assert!(
        s.wait_for("1. Yes", card),
        "the egress approval card should render for curl inside a worktree:\n{}",
        s.output()
    );
    assert!(
        s.output_plain().contains("curl"),
        "the card should show the curl command:\n{}",
        s.output()
    );
    drop(s);
    std::fs::remove_dir_all(&repo).ok();
}

/// PTY test: /trajectory on a fresh session with no turns yet. The
/// turn-list level must render the "0 turns" header, and Enter must NOT
/// drill into the turn-detail level — drilling into an empty row list
/// rendered "no row data" (the crash a user hit). The drill guard blocks
/// the Enter when the row list is empty, so the pane stays at the turn-list
/// level with no crash.
#[test]
#[ignore]
fn test_trajectory_session_no_crash() {
    let mut s = session_on_working();
    s.send_str("/trajectory");
    s.send_key(&Key::Enter);
    let t = std::time::Duration::from_secs(5);
    assert!(
        s.wait_for_plain("turns", t),
        "trajectory summary must render:
{}",
        s.output()
    );
    // The turn-list level renders its footer hint (Enter is a no-op here when
    // the row list is empty — the guard holds the pane at this level).
    let plain = s.output_plain();
    assert!(
        plain.contains("expand"),
        "turn-list footer must render: {plain}"
    );
    // Enter on an empty row list must NOT drill — the guard holds. The
    // crash was "no row data" rendering at the turn-detail level after
    // drilling into zero rows; it must not appear.
    s.clear_output();
    s.send_key(&Key::Enter);
    std::thread::sleep(std::time::Duration::from_millis(300));
    let plain = s.output_plain();
    assert!(
        !plain.contains("no row data"),
        "drilling into an empty turn list must not render the no-row-data crash state: {plain}"
    );
    // Esc closes the pane, returning to the working surface.
    s.clear_output();
    s.send_key(&Key::Esc);
    std::thread::sleep(std::time::Duration::from_millis(200));
    let plain = s.output_plain();
    assert!(
        !plain.contains("turns"),
        "Esc must close the trajectory pane: {plain}"
    );
    drop(s);
}

/// A grep whose path is outside the workspace raises the path-bounds approval
/// card (Detection, mode-immune — fires under the default-open posture too,
/// not just Manual). The card fires before the tool runs. Approving
/// "yes-don't-ask" (option 2) grants the directory to the fence + persists
/// it, the run resumes, grep runs against the now-authorized outside path,
/// and the transcript ends clean — no stale error. Pins the ask -> approve ->
/// directory-consent -> resume chain through the real binary; the unit layer
/// covers only the decide + route logic. Runs in a throwaway repo so the
/// persisted directory grant lands in that repo's local permissions store,
/// not the development tree's.
#[test]
#[ignore]
fn test_grep_outside_approves_resumes() {
    let repo = make_temp_repo(7);
    let outside = std::env::temp_dir().join(format!("houyi-buga-outside-{}-7", std::process::id()));
    drop(std::fs::remove_dir_all(&outside));
    std::fs::create_dir_all(&outside).expect("mkdir outside");
    std::fs::write(outside.join("target.txt"), "matchme here\n").expect("write target");
    let script = format!(
        r#"[
  [{{"type":"ToolCall","id":"c1","name":"grep","input":{{"pattern":"matchme","path":"{outside}"}}}}],
  [{{"type":"Text","text":"done"}}]
]"#,
        outside = outside.display(),
    );
    let mut s = session_on_working_in_repo(repo.clone(), &script);
    s.send_str("go");
    s.send_key(&Key::Enter);
    // The path-bounds Detection ask is mode-immune, so the card fires even
    // under the default-open (Auto) posture.
    let card = Duration::from_secs(20);
    assert!(
        s.wait_for("1. Yes", card),
        "the path-bounds approval card should render for an outside grep:\n{}",
        s.output()
    );
    // '2' selects Yes-don't-ask (always): grants the directory + persists it
    // so a later grep on the same path rehydrates without re-asking.
    s.send_str("2");
    s.send_key(&Key::Enter);
    let end = Duration::from_secs(15);
    assert!(
        s.wait_for("done", end),
        "approving should resume the run + render done:\n{}",
        s.output()
    );
    assert!(
        !s.output().contains("error:"),
        "the transcript must not leave a stale error after approving the outside grep:\n{}",
        s.output()
    );
    std::fs::remove_dir_all(&outside).ok();
}
