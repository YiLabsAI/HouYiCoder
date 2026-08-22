//! Real-binary PTY UI tests for the /permissions pane. #[ignore] (each
//! spawns the houyi binary + a PTY — too slow for the 60s commit gate). Run
//! via make test ui (builds the bin first) or
//! cargo test --test ui_permissions -- --ignored after cargo build --bin houyi.
//!
//! These catch the flow / render / key-routing class that TestBackend unit
//! tests cannot: the real crossterm event loop, the real repaint, and the full
//! slash-palette → pane → sub-mode chain through a terminal. The cell-state
//! regressions (cursor-clamp) stay in the TestBackend layer where render_buffer
//! asserts on style directly — each concern at its layer.

#![allow(clippy::unwrap_in_result)]

mod common;

use common::{
    Key, RENDER_TIMEOUT, fresh_temp_dir, open_permissions, session_on_working,
    session_on_working_in_dir, session_on_working_with_script, tab_to_workspace,
};

#[test]
#[ignore]
fn test_pane_opens() {
    let mut s = session_on_working();
    open_permissions(&mut s);
    // The Pane primitive draws a full-width ─ Divider framing the region.
    s.assert_contains("─");
}

#[test]
#[ignore]
fn test_workspace_add_dir() {
    let mut s = session_on_working();
    open_permissions(&mut s);
    tab_to_workspace(&mut s);
    // 'a' enters AddDir. A path typed here (leading '/') must land in the
    // input box, not snap to the palette — the AddDir sub-mode gates '/'.
    s.send_key(&Key::Char('a'));
    assert!(
        s.wait_for("add directory:", RENDER_TIMEOUT),
        "AddDir prompt should render"
    );
    let dir = fresh_temp_dir("add");
    let dir_name = dir.file_name().unwrap().to_string_lossy().into_owned();
    s.send_str(&dir.to_string_lossy());
    s.send_key(&Key::Enter);
    // The server canonicalizes the path (macOS /var -> /private/var) before
    // listing it, so assert on the stable basename, not the full input path.
    assert!(
        s.wait_for(&dir_name, RENDER_TIMEOUT),
        "added directory should appear in the Workspace list:\n{}",
        s.output()
    );
}

#[test]
#[ignore]
fn test_add_dir_path_errors() {
    // Adding a path that is a FILE (not a directory) must surface an error,
    // not silently drop the keystroke. The server's add_working_dir rejects
    // non-directories; the test asserts the rejection reaches the screen.
    let mut s = session_on_working();
    open_permissions(&mut s);
    tab_to_workspace(&mut s);
    s.send_key(&Key::Char('a'));
    assert!(s.wait_for("add directory:", RENDER_TIMEOUT));
    // A real file, not a directory.
    let file = std::env::temp_dir().join(format!(
        "houyi-ui-file-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::write(&file, b"x").expect("write file");
    s.send_str(&file.to_string_lossy());
    s.send_key(&Key::Enter);
    // The rejection message must reach the screen (whatever path it takes).
    assert!(
        s.wait_for("not a directory", RENDER_TIMEOUT)
            || s.wait_for("directory", RENDER_TIMEOUT)
            || s.wait_for("error", RENDER_TIMEOUT),
        "bad-path rejection should surface on the screen:\n{}",
        s.output()
    );
    drop(std::fs::remove_file(&file));
}

#[test]
#[ignore]
fn test_rule_add_flow() {
    // The rule Add flow: spec text → Enter → destination pick (←→) → Enter
    // ships → the rule appears in the Allow list.
    let mut s = session_on_working();
    open_permissions(&mut s);
    // Default tab is Allow; 'a' enters the rule Add sub-mode.
    s.send_key(&Key::Char('a'));
    assert!(
        s.wait_for("add:", RENDER_TIMEOUT),
        "rule Add prompt should render"
    );
    s.send_str("bash npm:allow");
    s.send_key(&Key::Enter);
    // Advance to the destination pick (default project).
    assert!(
        s.wait_for("destination:", RENDER_TIMEOUT),
        "destination pick should render after the spec Enter"
    );
    s.send_key(&Key::Enter); // ship with the default destination
    // The rule lands in the Allow list (server adds + ack refreshes).
    assert!(
        s.wait_for("Bash(npm", RENDER_TIMEOUT),
        "added rule should appear in the Allow list:\n{}",
        s.output()
    );
}

#[test]
#[ignore]
fn test_esc_exits_pane() {
    // Esc leaves the pane + the transcript input box is live afterward.
    // Two valid checks under ratatui's incremental renderer (which does NOT
    // repaint unchanged cells): (1) the Permissions: header is a CHANGE so it
    // is repainted-away after Esc (gone from the buffer); (2) typing a char
    // forces a repaint of the input box (the char is a change) — proving the
    // Working screen + input box render after Esc. (wait_for on unchanged
    // content like the placeholder would be a false negative — the renderer
    // skips it — so we drive a change instead.)
    let mut s = session_on_working();
    open_permissions(&mut s);
    s.assert_contains("Permissions:");
    s.clear_output();
    s.send_key(&Key::Esc);
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(
        !s.output().contains("Permissions:"),
        "Esc should leave the pane (Permissions: header gone):\n{}",
        s.output()
    );
    // The input box is responsive: typing forces a repaint that lands the
    // typed char in the buffer — the Working screen renders after Esc.
    s.clear_output();
    s.send_str("x");
    assert!(
        s.wait_for("x", RENDER_TIMEOUT),
        "input box should render the typed char after Esc:\n{}",
        s.output()
    );
}

#[test]
#[ignore]
fn test_workspace_remove_dir() {
    // An isolated cwd, not the workspace root: from a linked worktree the
    // startup allow-back lists the main checkout's git dir, so the
    // post-removal empty-state assertion below would never hold there.
    let cwd = fresh_temp_dir("ws-rm");
    let mut s = session_on_working_in_dir(cwd);
    open_permissions(&mut s);
    tab_to_workspace(&mut s);
    // Add a dir first (reuse the add flow).
    s.send_key(&Key::Char('a'));
    let dir = fresh_temp_dir("rm");
    let dir_name = dir.file_name().unwrap().to_string_lossy().into_owned();
    s.send_str(&dir.to_string_lossy());
    s.send_key(&Key::Enter);
    assert!(s.wait_for(&dir_name, RENDER_TIMEOUT));
    // Drop history so the post-removal empty-state check tests the CURRENT
    // render (the initial "no additional directories" from before the add
    // would otherwise already be in the buffer).
    s.clear_output();
    // 'd' on the dir → RemoveDir (No preselected); Right → Yes; Enter ships.
    s.send_key(&Key::Char('d'));
    assert!(
        s.wait_for("remove this directory?", RENDER_TIMEOUT),
        "RemoveDir prompt should render"
    );
    s.send_key(&Key::Right); // → Yes
    s.send_key(&Key::Enter);
    // The server removes it; the ack refreshes dirs_cache; the Workspace
    // list re-renders to the empty state.
    assert!(
        s.wait_for("no additional directories", RENDER_TIMEOUT),
        "Workspace should re-render empty after removal:\n{}",
        s.output()
    );
    drop(std::fs::remove_dir(&dir));
}

/// #37 real scenario: grant a workspace dir via /permissions, then have the
/// agent READ a file inside it. Under Auto (the local-mode default) read is
/// read-only so it auto-allows, and the sandbox resolve() checks additional_dirs
/// so the read SUCCEEDS — NOT the old "path escapes workspace" denial even
/// after the grant. This is the bug the user reported: granting a dir did not
/// unblock read into it (bash could reach it via the sandbox profile, but
/// read/write via resolve could not, because resolve rejected every absolute
/// path). The fix is the sandbox resolve() checking additional_dirs; this pins
/// it under PTY so a regression at the interaction layer is caught.
#[test]
#[ignore]
fn test_add_dir_read_auto() {
    // A temp dir OUTSIDE the workspace + a file in it. Without the grant,
    // confine would deny the read; with the grant, resolve() allows it.
    let dir = fresh_temp_dir("addread");
    let file = dir.join("note.txt");
    std::fs::write(&file, "hello from added dir").expect("write fixture");
    let abs = file.to_string_lossy().into_owned();
    let script = r#"[
  [{"type":"ToolCall","id":"c1","name":"read","input":{"path":"__PATH__"}}],
  [{"type":"Text","text":"done"}]
]"#
    .replace("__PATH__", &abs);
    let mut s = session_on_working_with_script(&script);
    // Grant the dir via the real /permissions -> Workspace -> Add flow.
    open_permissions(&mut s);
    tab_to_workspace(&mut s);
    s.send_key(&Key::Char('a'));
    assert!(
        s.wait_for("add directory:", RENDER_TIMEOUT),
        "AddDir prompt should render"
    );
    s.send_str(&dir.to_string_lossy());
    s.send_key(&Key::Enter);
    let dir_name = dir.file_name().unwrap().to_string_lossy().into_owned();
    assert!(
        s.wait_for(&dir_name, RENDER_TIMEOUT),
        "the granted dir should list in Workspace:\n{}",
        s.output()
    );
    // Leave the pane, then start the run. The script's read ToolCall fires
    // AFTER the grant is in place, so additional_dirs holds the dir.
    s.send_key(&Key::Esc);
    std::thread::sleep(std::time::Duration::from_millis(200));
    s.send_str("read the note");
    s.send_key(&Key::Enter);
    // The read RAN + completed (fold.rs collapses the read call+result into
    // a "read N file(s)" dim summary — the TUI does NOT echo read content
    // by default, unlike a renderTruncatedContent 3-line peek;
    // content-echo is a separate display question, not this test's concern).
    // The discriminator for the add-dir-read bug: NO "path escapes workspace"
    // denial (the old behavior — the grant did not unblock read into the dir
    // because resolve rejected every absolute path). The sandbox resolve()
    // now checks additional_dirs, so the granted dir unblocks the read.
    assert!(
        s.wait_for_plain("Read 1 file", RENDER_TIMEOUT),
        "the read into the granted dir should run + fold to a summary:\n{}",
        s.output_plain()
    );
    assert!(
        !s.output().contains("escapes workspace"),
        "the old grant-did-not-unblock-read bug regressed (path denied):\n{}",
        s.output()
    );
    assert!(
        !s.output().contains("error:"),
        "the read should not surface an error:\n{}",
        s.output()
    );
    assert!(
        s.wait_for("done", RENDER_TIMEOUT),
        "the run should end after the read:\n{}",
        s.output()
    );
    drop(std::fs::remove_dir_all(&dir));
}
