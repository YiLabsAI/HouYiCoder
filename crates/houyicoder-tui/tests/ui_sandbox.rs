//! Real-binary PTY journeys for the /sandbox pane: the two tabs and the keys
//! that walk them, /rewind converging on the checkpoints tab, and the restore
//! confirmation from the key that opens it to the file on disk it changes.
//! Only the binary shows which screen a real key reaches and whether the
//! restore lands on the target the user was reading.

#![allow(clippy::unwrap_in_result)]

use std::path::{Path, PathBuf};
use std::process::id;
use std::thread;
use std::time::{Duration, Instant};

use crate::common::{
    Key, PtySession, RENDER_TIMEOUT, make_temp_repo, pty_session_in_repo, run_slash_command,
    wait_file_contains,
};

/// Script template: a scripted write that creates a file the session then
/// records, followed by plain text so the run ends on the second reply. Built
/// with replace so the JSON braces need no escaping.
const WRITE_SCRIPT_TEMPLATE: &str = r#"[
  [{"type":"ToolCall","id":"c1","name":"write","input":{"path":"__PATH__","content":"scout sheet\n"}}],
  [{"type":"Text","text":"done"}]
]"#;

fn write_script(rel_path: &str) -> String {
    WRITE_SCRIPT_TEMPLATE.replace("__PATH__", rel_path)
}

/// Poll the screen until the marker is gone. The accumulated byte stream keeps
/// every render it ever saw, so a closed pane is invisible there; only the
/// reconstructed screen says what is up now.
fn wait_absent(s: &mut PtySession, marker: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !s.screen().contents().contains(marker) {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    !s.screen().contents().contains(marker)
}

/// Poll until the pane holds a selectable row for the target. The transcript
/// above the pane keeps its own copy of the path from the write, so matching
/// the path alone finds a line the pane never drew; the row marker is only
/// there once the listing has rendered a row to act on.
fn wait_for_row(s: &mut PtySession, file: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let screen = s.screen().contents();
        let found = screen
            .lines()
            .any(|line| line.contains("❯") && line.contains(file));
        if found || Instant::now() >= deadline {
            return found;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Poll until the file is gone, which the restore reaches only after the host
/// has applied it.
fn wait_file_absent(path: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !path.exists() {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    !path.exists()
}

/// One launch lands a recorded write the checkpoints tab can list, in the
/// directory the binary runs from.
fn session_with_one_write(slug: &str, file: &str) -> (PtySession, PathBuf) {
    let repo = make_temp_repo(slug);
    let rel = format!("target/{file}");
    let abs = repo.join("target").join(file);
    let mut s = pty_session_in_repo(repo, &write_script(&rel));
    s.send_str("write the scout sheet");
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for("done", RENDER_TIMEOUT),
        "the scripted run should end after the write:\n{}",
        s.output()
    );
    assert!(
        wait_file_contains(&abs, "scout sheet", RENDER_TIMEOUT),
        "the write should land before the pane lists it: {}",
        abs.display()
    );
    (s, abs)
}

/// The pane opens on the sandbox tab, Tab walks to the checkpoints tab and
/// back, Esc closes it at the root, and /rewind opens the checkpoints tab
/// directly. Opening the recorded writes changes nothing on disk, since a
/// rollback is only ever the confirmation the user picks.
#[test]
#[ignore]
fn test_tabs_and_rewind() {
    let file = format!("tabs-{}.txt", id());
    let (mut s, abs) = session_with_one_write("sandbox-tabs", &file);
    run_slash_command(&mut s, "sandbox");
    assert!(
        s.wait_for_screen("[Sandbox]", RENDER_TIMEOUT),
        "/sandbox should open the sandbox tab:\n{}",
        s.screen().contents()
    );
    s.send_key(&Key::Tab);
    assert!(
        wait_for_row(&mut s, &file, RENDER_TIMEOUT),
        "Tab should walk to the recorded writes:\n{}",
        s.screen().contents()
    );
    s.send_key(&Key::Tab);
    assert!(
        s.wait_for_screen("[Sandbox]", RENDER_TIMEOUT),
        "Tab should walk back to the sandbox tab:\n{}",
        s.screen().contents()
    );
    s.send_key(&Key::Esc);
    assert!(
        wait_absent(&mut s, "switch tab", RENDER_TIMEOUT),
        "Esc at the sandbox root should close the pane:\n{}",
        s.screen().contents()
    );
    run_slash_command(&mut s, "rewind");
    assert!(
        s.wait_for_screen("[Checkpoints]", RENDER_TIMEOUT),
        "/rewind should open the recorded writes:\n{}",
        s.screen().contents()
    );
    assert!(
        !s.screen().contents().contains("[Sandbox]"),
        "/rewind reads the checkpoints tab, not the sandbox one:\n{}",
        s.screen().contents()
    );
    assert!(
        abs.exists(),
        "/rewind lists the writes; only a confirmation restores one"
    );
}

/// The confirmation starts on the button that changes nothing: Enter closes it
/// and the file the write created is still there. That the button asks the
/// host for nothing is pinned where the request is made, since a screen can
/// only show what was drawn, not what a late answer would have drawn.
#[test]
#[ignore]
fn test_restore_default_cancel() {
    let file = format!("cancel-{}.txt", id());
    let (mut s, abs) = session_with_one_write("sandbox-cancel", &file);
    run_slash_command(&mut s, "sandbox");
    assert!(
        s.wait_for_screen("[Sandbox]", RENDER_TIMEOUT),
        "{}",
        s.screen().contents()
    );
    s.send_key(&Key::Tab);
    assert!(
        wait_for_row(&mut s, &file, RENDER_TIMEOUT),
        "the listing should draw a row for the recorded write:\n{}",
        s.screen().contents()
    );
    s.send_key(&Key::Char('r'));
    assert!(
        s.wait_for_screen("[Cancel]", RENDER_TIMEOUT),
        "r should open the restore confirmation:\n{}",
        s.screen().contents()
    );
    assert!(
        s.wait_for_screen("removes the file this write created", RENDER_TIMEOUT),
        "the confirmation should say what this restore does:\n{}",
        s.screen().contents()
    );
    s.send_key(&Key::Enter);
    assert!(
        wait_absent(&mut s, "[Cancel]", RENDER_TIMEOUT),
        "Enter on the default button should leave the confirmation:\n{}",
        s.screen().contents()
    );
    s.send_key(&Key::Esc);
    assert!(
        wait_absent(&mut s, "switch tab", RENDER_TIMEOUT),
        "Esc at the listing should close the pane:\n{}",
        s.screen().contents()
    );
    assert!(abs.exists(), "the default button restores nothing");
}

/// A restore the user moves the focus to and confirms removes the file the
/// write created, and the outcome reaches the transcript, where it outlives
/// the pane.
#[test]
#[ignore]
fn test_restore_removes_new_file() {
    let file = format!("remove-{}.txt", id());
    let (mut s, abs) = session_with_one_write("sandbox-remove", &file);
    run_slash_command(&mut s, "sandbox");
    assert!(
        s.wait_for_screen("[Sandbox]", RENDER_TIMEOUT),
        "{}",
        s.screen().contents()
    );
    s.send_key(&Key::Tab);
    assert!(
        wait_for_row(&mut s, &file, RENDER_TIMEOUT),
        "the listing should draw a row for the recorded write:\n{}",
        s.screen().contents()
    );
    s.send_key(&Key::Char('r'));
    assert!(
        s.wait_for_screen("[Cancel]", RENDER_TIMEOUT),
        "r should open the restore confirmation:\n{}",
        s.screen().contents()
    );
    s.send_key(&Key::Right);
    assert!(
        s.wait_for_screen("[Restore checkpoint]", RENDER_TIMEOUT),
        "Right should move the focus to the restore button:\n{}",
        s.screen().contents()
    );
    s.send_key(&Key::Enter);
    assert!(
        wait_absent(&mut s, "[Restore checkpoint]", RENDER_TIMEOUT),
        "the confirmation should close once the host answers:\n{}",
        s.screen().contents()
    );
    s.send_key(&Key::Esc);
    assert!(
        s.wait_for_screen("checkpoint: restored", RENDER_TIMEOUT),
        "the outcome should reach the transcript:\n{}",
        s.screen().contents()
    );
    assert!(
        wait_file_absent(&abs, RENDER_TIMEOUT),
        "the restore should remove the file the write created: {}",
        abs.display()
    );
}
