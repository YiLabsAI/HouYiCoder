//! Prompt migration PTY journeys: a session recorded before prompts were
//! recorded is untitled until the background pass reads its log and fills the
//! record, and the picker and status page then answer from that record.

use std::fs;
use std::thread;
use std::time::{Duration, Instant};

use crate::common::{self, Key, PtySession, RENDER_TIMEOUT, fresh_temp_dir, run_slash_command};

/// A session recorded before prompts were recorded states no title, and the
/// picker answers from records alone. The background pass reads the session's
/// log, writes the record, and the row then carries the user's own words;
/// opening the picker itself opens no log.
#[test]
#[ignore]
fn test_picker_titles_legacy_session() {
    let sessions_dir = fresh_temp_dir("sessions-legacy-title");
    let sid = "99999999-9999-4999-8999-999999999999";
    common::seed_session_turns_on_disk(
        &sessions_dir,
        sid,
        "legacy-title-model",
        &["fix the flaky login test"],
    );
    let record = sessions_dir.join(sid).join("session.json");
    let mut s = PtySession::launch_with_sessions_dir(None, None, None, None, &[], sessions_dir);
    assert!(s.wait_for("sign in to houyicoder", RENDER_TIMEOUT));
    s.send_key(&Key::Char('3'));
    assert!(
        s.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "working screen after local login:\n{}",
        s.output()
    );
    // The record gaining the statement is the pass's effect on disk, so the
    // picker opens after the migration rather than racing it.
    assert!(
        common::wait_file_contains(&record, "fix the flaky login test", Duration::from_secs(15)),
        "the pass should fill the record:\n{}",
        fs::read_to_string(&record).unwrap_or_default()
    );
    run_slash_command(&mut s, "resume");
    // The picker header is a rendered row: a repaint writes only the cells
    // that changed, so the byte stream can splice words. The rebuilt screen
    // is what the user reads.
    assert!(
        s.wait_for_screen("Resume a session", RENDER_TIMEOUT),
        "picker should open:\n{}",
        s.screen().contents()
    );
    assert!(
        s.wait_for_screen("fix the flaky login test", RENDER_TIMEOUT),
        "the row should carry the migrated title:\n{}",
        s.screen().contents()
    );
}

/// The same migration under --resume: the opened session's record states no
/// name when the session starts, and /status names it once the pass has read
/// the log. The status answer comes from the record alone, so this is the
/// path that proves the record is what filled in.
#[test]
#[ignore]
fn test_status_names_legacy_session() {
    let sessions_dir = fresh_temp_dir("sessions-legacy-status");
    let sid = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    common::seed_session_turns_on_disk(
        &sessions_dir,
        sid,
        "legacy-status-model",
        &["rename the deployment script"],
    );
    let record = sessions_dir.join(sid).join("session.json");
    let mut s = PtySession::launch_with_sessions_dir(
        None,
        None,
        None,
        None,
        &["--resume".to_string(), sid.to_string()],
        sessions_dir,
    );
    assert!(
        s.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "working screen after resume:\n{}",
        s.output()
    );
    assert!(
        common::wait_file_contains(
            &record,
            "rename the deployment script",
            Duration::from_secs(15)
        ),
        "the pass should fill the record:\n{}",
        fs::read_to_string(&record).unwrap_or_default()
    );
    run_slash_command(&mut s, "status");
    // The name row, not the replayed transcript: the resumed log also holds
    // the statement as a user message, so a screen-wide match would pass
    // whatever the record said.
    let deadline = Instant::now() + RENDER_TIMEOUT;
    loop {
        let screen = s.screen().contents();
        if screen.lines().any(|line| {
            let line = line.trim_start();
            line.starts_with("Session name") && line.contains("rename the deployment script")
        }) {
            break;
        }
        if Instant::now() > deadline {
            panic!("/status should name the session from its filled record:\n{screen}");
        }
        thread::sleep(Duration::from_millis(20));
    }
}
