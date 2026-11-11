//! Resume PTY journey tests that drive the real binary: the sid-resume, lock
//! release (clean exit + crash), status provenance, stale-cwd-degraded, and
//! legacy-record prompt migration journeys. Fixtures are shared via the common
//! module.

#![allow(clippy::unwrap_in_result)]

use dunce::canonicalize;
use std::env;
use std::fs;
use std::fs::FileTimes;
use std::fs::OpenOptions;
use std::fs::remove_dir_all;
use std::path::Path;
use std::process;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use houyicoder_core::{EventId, SessionEvent, SessionId, SessionLogEntry};

use crate::common::{self, Key, PtySession, RENDER_TIMEOUT, fresh_temp_dir, run_slash_command};

/// --resume <sid> re-opens an existing session: the sid is REUSED (not a
/// fork), the model is restored from the descriptor, the seeded history stays,
/// and a continued turn appends to the SAME sid's log. The success path the
/// missing-sid error test does not cover; the file-branch test covers a fork
/// (new sid), this covers the reuse invariant.
#[test]
#[ignore]
fn test_resume_sid_reopens_history() {
    let sessions_dir = fresh_temp_dir("sessions-sid-reopen");
    let sid = "33333333-3333-3333-3333-333333333333";
    common::seed_session_on_disk(&sessions_dir, sid, "sid-reopen-model", "seeded prompt");
    let mut s = PtySession::launch_with_sessions_dir(
        None,
        None,
        None,
        None,
        &["--resume".to_string(), sid.to_string()],
        sessions_dir.clone(),
    );
    assert!(
        s.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "working screen after sid resume"
    );
    // Poll for the model label rather than reading the output once: the
    // placeholder can reach the PTY ahead of the status bar's bytes, so a
    // single read taken the moment the placeholder matches can miss the
    // label under contention.
    assert!(
        s.wait_for("sid-reopen-model", RENDER_TIMEOUT),
        "model should be restored from the descriptor:\n{}",
        s.output()
    );
    // The seeded history must render in the transcript, not just persist to
    // disk — the server replays the durable trajectory on connect so the
    // working screen shows the resumed conversation (resumed messages
    // load into the message array on connect).
    assert!(
        s.wait_for("seeded prompt", RENDER_TIMEOUT),
        "resumed history should be visible in the transcript:\n{}",
        s.output()
    );
    let log_path = sessions_dir.join(sid).join("log.jsonl");
    let before = fs::read_to_string(&log_path).unwrap();
    assert!(
        before.contains("seeded prompt"),
        "seeded history should still be in the log:\n{before}"
    );
    let before_lines = before.lines().count();
    s.send_str("after reopen");
    s.send_key(&Key::Enter);
    let deadline = Instant::now() + RENDER_TIMEOUT;
    let after = loop {
        let after = fs::read_to_string(&log_path).unwrap();
        if after.lines().count() > before_lines {
            break after;
        }
        if Instant::now() > deadline {
            panic!("a continued turn should append to the same sid log:\n{after}");
        }
        thread::sleep(Duration::from_millis(50));
    };
    assert!(
        after.contains("after reopen"),
        "the new turn should be in the reused sid log:\n{after}"
    );
}

/// A resumed session's Usage tab reports the spend its history already holds:
/// the turns the log carried are part of the session total, not only the turns
/// run after the reopen. A tally that started at zero reported the seeded turn
/// as nothing and left a session that recorded a cache share without one.
#[test]
#[ignore]
fn test_resume_usage_covers_history() {
    let sessions_dir = fresh_temp_dir("sessions-usage-history");
    let sid = "44444444-4444-4444-4444-444444444444";
    common::seed_session_with_usage(
        &sessions_dir,
        sid,
        "usage-model",
        "seeded prompt",
        common::SeededTurn {
            input: 12_000,
            output: 500,
            cache_read: 9_600,
        },
    );
    let mut s = PtySession::launch_with_sessions_dir(
        None,
        None,
        None,
        None,
        &["--resume".to_string(), sid.to_string()],
        sessions_dir.clone(),
    );
    assert!(
        s.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "working screen after sid resume:\n{}",
        s.output()
    );
    // The status bar shows the model the descriptor restored, so a status
    // snapshot has landed before the Usage tab is opened over it.
    assert!(
        s.wait_for_plain("usage-model", RENDER_TIMEOUT),
        "the restored model reaches the status bar:\n{}",
        s.output()
    );
    run_slash_command(&mut s, "status");
    assert!(
        s.wait_for_plain("Status", RENDER_TIMEOUT),
        "/status renders the Status tab:\n{}",
        s.output_plain()
    );
    // Tab cycles: Status -> Config -> Usage.
    s.send_key(&Key::Tab);
    s.send_key(&Key::Tab);
    // The Usage tab's rows are one row each: the token row and the cache row
    // the recorded turn produced.
    assert!(
        s.wait_for_screen("Input tokens", RENDER_TIMEOUT),
        "the Usage tab renders the token rows:\n{}",
        s.screen().contents()
    );
    assert!(
        s.wait_for_screen("9.6k (80.0% of input)", RENDER_TIMEOUT),
        "the Usage tab reports the recorded turn's cache share:\n{}",
        s.screen().contents()
    );
    assert!(
        s.wait_for_screen("12k", RENDER_TIMEOUT),
        "the Usage tab reports the recorded turn's input:\n{}",
        s.screen().contents()
    );
}

/// A session directory named in the ULID spelling still resumes: the store
/// keeps such a directory under its old name, so a --resume by the current
/// display form must find it rather than report a missing session. The unit
/// layer covers the id round-trip; this drives the real binary, the store
/// lookup, and the replay that follows.
#[test]
#[ignore]
fn test_resume_legacy_dir() {
    let sessions_dir = fresh_temp_dir("sessions-legacy-name");
    let uuid_str = "5f1c2b3a-9d4e-4a71-8c62-0e5b7d3f1a90";
    let sid = SessionId::from_display_string(uuid_str).expect("sid parses");
    // The store's own name for this id: a ULID, never the hyphenated display
    // form, so the seeded directory differs from the id the launch passes.
    let legacy_name = sid.ulid_name();
    common::seed_session_on_disk(&sessions_dir, &legacy_name, "legacy-model", "legacy prompt");
    assert!(
        sessions_dir.join(&legacy_name).join("log.jsonl").is_file(),
        "the seed must land in the legacy-named directory"
    );
    let mut s = PtySession::launch_with_sessions_dir(
        None,
        None,
        None,
        None,
        &["--resume".to_string(), uuid_str.to_string()],
        sessions_dir.clone(),
    );
    assert!(
        s.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "resuming a legacy-named session reaches the working screen:\n{}",
        s.output()
    );
    assert!(
        s.wait_for_plain("legacy-model", RENDER_TIMEOUT),
        "the descriptor in the legacy-named directory is read:\n{}",
        s.output()
    );
    assert!(
        s.wait_for("legacy prompt", RENDER_TIMEOUT),
        "the legacy-named session's history replays:\n{}",
        s.output()
    );
}

/// The file lock is released on a clean exit: binary 1 --resume <sid> holds
/// the lock, quits cleanly (ctrl+D double-press on the working screen), then
/// binary 2 --resume <sid> acquires the released lock. The lock-rejects-second
/// test covers the contention case; this covers the release-on-exit invariant.
#[test]
#[ignore]
fn test_resume_lock_released_exit() {
    let sessions_dir = fresh_temp_dir("sessions-lock-release");
    let sid = "44444444-4444-4444-4444-444444444444";
    common::seed_session_on_disk(&sessions_dir, sid, "lock-release-model", "locked session");
    let mut s1 = PtySession::launch_with_sessions_dir(
        None,
        None,
        None,
        None,
        &["--resume".to_string(), sid.to_string()],
        sessions_dir.clone(),
    );
    // b1 holds the lock on the resumed session (working screen). Quit it
    // cleanly via ctrl+D double-press so the lock is released for b2.
    s1.send_key(&Key::Ctrl('d'));
    s1.send_key(&Key::Ctrl('d'));
    // The lock is released when the process exits, so the exit is the latch.
    assert!(
        s1.wait_for_exit(RENDER_TIMEOUT),
        "ctrl+D twice should exit the first process:\n{}",
        s1.output()
    );
    let mut s2 = PtySession::launch_with_sessions_dir(
        None,
        None,
        None,
        None,
        &["--resume".to_string(), sid.to_string()],
        sessions_dir.clone(),
    );
    assert!(
        s2.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "b2 should reach working after b1 released the lock:\n{}",
        s2.output()
    );
    assert!(
        !s2.output().contains("open in another process"),
        "b2 must not be rejected after b1 clean exit:\n{}",
        s2.output()
    );
}

/// /status shows provenance=resumed after a --resume <file> launch (the
/// descriptor carries ResumedFromExport, mapped to the wire + rendered).
#[test]
#[ignore]
fn test_status_shows_resumed_provenance() {
    let fixture = common::write_resume_fixture();
    // A taller terminal so the status pane admits the full field set (at 24
    // rows the lower lines including provenance clip).
    let sessions_dir = common::fresh_temp_dir("sessions-resume-provenance");
    let mut s = PtySession::launch_with_sessions_dir_rows(
        None,
        None,
        None,
        None,
        &[
            "--resume".to_string(),
            fixture.to_string_lossy().into_owned(),
        ],
        sessions_dir,
        40,
    );
    assert!(
        s.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "working screen after export resume"
    );
    run_slash_command(&mut s, "status");
    // Use the ANSI-stripped form: the pane renderer emits cursor-positioning
    // escapes between styled spans, so multi-word labels are not contiguous
    // in the raw byte stream.
    assert!(
        s.wait_for_plain("Provenance:", RENDER_TIMEOUT),
        "/status should render the provenance line:\n{}",
        s.output()
    );
    let out = s.output_plain();
    // "resumed from export" has spaces the pane renderer splits across cursor
    // jumps; assert the single-word tokens that identify the provenance kind.
    assert!(
        out.contains("resumed"),
        "provenance should be resumed-from-export:\n{out}"
    );
    assert!(out.contains("export"), "export provenance:\n{out}");
}

/// The file lock is released on a CRASH (SIGKILL): binary 1 is hard-killed
/// without running Drop, then binary 2 acquires the OS-released flock. The
/// clean-exit test covers the Drop path; this covers the OS-releases-on-death
/// invariant (a stale lock after a crash would block that sid forever).
#[test]
#[ignore]
fn test_resume_lock_released_crash() {
    let sessions_dir = fresh_temp_dir("sessions-lock-crash");
    let sid = "55555555-5555-5555-5555-555555555555";
    common::seed_session_on_disk(&sessions_dir, sid, "crash-model", "crash session");
    let mut s1 = PtySession::launch_with_sessions_dir(
        None,
        None,
        None,
        None,
        &["--resume".to_string(), sid.to_string()],
        sessions_dir.clone(),
    );
    s1.kill_hard();
    // A hard kill releases the lock at process teardown, so wait for the
    // process to go before the second launch claims it.
    assert!(
        s1.wait_for_exit(RENDER_TIMEOUT),
        "the killed process should be reaped:\n{}",
        s1.output()
    );
    let mut s2 = PtySession::launch_with_sessions_dir(
        None,
        None,
        None,
        None,
        &["--resume".to_string(), sid.to_string()],
        sessions_dir.clone(),
    );
    assert!(
        s2.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "b2 should reach working after b1 crashed + OS released the lock:\n{}",
        s2.output()
    );
    assert!(
        !s2.output().contains("open in another process"),
        "b2 must not be rejected after b1 crash:\n{}",
        s2.output()
    );
}

/// --continue converges strictly on the current workspace: a session whose
/// recorded cwd is a DIFFERENT path is filtered out, and --continue errors
/// with "no session" rather than silently jumping into another repo's
/// session. Discriminates the cwd filter itself: continue_no_session_errors
/// seeds nothing, so None comes back for absence (a removed cwd filter would
/// still return None + the test would still pass -- not falsifiable). This
/// seeds an other-cwd session with a real log + meta, so a broken or removed
/// cwd filter would pick it (resume it) and this test would fail. Falsifies
/// the proof-token risk the empty-case test carries.
#[test]
#[ignore]
fn test_continue_rejects_cwd_session() {
    let sessions_dir = fresh_temp_dir("sessions-continue-other-cwd");
    let sid = "77777777-7777-7777-7777-777777777777";
    common::seed_session_with_cwd(
        &sessions_dir,
        sid,
        "other-cwd-model",
        "other workspace prompt",
        "/nonexistent/houyi-other-workspace-test",
    );
    let mut s = PtySession::launch_with_sessions_dir(
        None,
        None,
        None,
        None,
        &["--continue".to_string()],
        sessions_dir,
    );
    assert!(
        s.wait_for_plain("no session in", RENDER_TIMEOUT),
        "--continue must reject an other-cwd session, not silently jump to it:\n{}",
        s.output_plain()
    );
}

/// Stamp a path's mtime to a fixed age, so the activity order the pick
/// depends on is deterministic: the listing resolves mtime at whole-second
/// granularity, and two seeds written in one second would tie.
fn stamp_age(path: &Path, secs_ago: u64) {
    let t = SystemTime::now() - Duration::from_secs(secs_ago);
    // Opened for write because windows stamps times through a handle that
    // carries the write-attributes right.
    OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open for set_times")
        .set_times(FileTimes::new().set_modified(t))
        .expect("set mtime");
}

/// --continue skips a session another process holds and resumes the next
/// newest one, so a second terminal continues older work instead of exiting
/// with a lock refusal.
#[test]
#[ignore]
fn test_continue_skips_held() {
    let sessions_dir = fresh_temp_dir("sessions-continue-held");
    // The binary converges --continue on its own workspace, so the seeds
    // record the workspace the child runs in, spelled as the child resolves
    // it.
    let ws = fresh_temp_dir("continue-ws");
    let cwd = canonicalize(&ws).expect("canonical workspace");
    let cwd_str = cwd.to_string_lossy().into_owned();
    let older = "88888888-8888-8888-8888-888888888888";
    let newer = "99999999-9999-9999-9999-999999999999";
    common::seed_session_with_cwd(
        &sessions_dir,
        older,
        "model-older",
        "older prompt",
        &cwd_str,
    );
    common::seed_session_with_cwd(
        &sessions_dir,
        newer,
        "model-newer",
        "newer prompt",
        &cwd_str,
    );
    stamp_age(&sessions_dir.join(older).join("log.jsonl"), 100);
    stamp_age(&sessions_dir.join(newer).join("log.jsonl"), 10);

    // Hold the newest session from this process, a different process from
    // the binary under test.
    let lock = common::hold_session_lock(&sessions_dir.join(newer).join("session.lock"));

    let mut s = PtySession::launch_with_sessions_dir(
        None,
        None,
        None,
        Some(ws.clone()),
        &["--continue".to_string()],
        sessions_dir.clone(),
    );
    assert!(
        s.wait_for("model-older", RENDER_TIMEOUT),
        "--continue should resume the newest free session:\n{}",
        s.output()
    );
    assert!(
        !s.output().contains("model-newer"),
        "--continue must not resume the held session:\n{}",
        s.output()
    );
    drop(lock);
    drop(s);
    remove_dir_all(ws).ok();
}

/// When every session of the workspace is held, --continue refuses before
/// opening a host and prints one line per session naming that session's fork
/// command, so the user has a way forward instead of an empty session.
#[test]
#[ignore]
fn test_continue_refuses_all_held() {
    let sessions_dir = fresh_temp_dir("sessions-continue-all-held");
    let ws = fresh_temp_dir("continue-all-held-ws");
    let cwd = canonicalize(&ws).expect("canonical workspace");
    let cwd_str = cwd.to_string_lossy().into_owned();
    let older = "66666666-6666-6666-6666-666666666666";
    let newer = "77777777-7777-7777-7777-777777777777";
    common::seed_session_with_cwd(
        &sessions_dir,
        older,
        "model-older",
        "older prompt",
        &cwd_str,
    );
    common::seed_session_with_cwd(
        &sessions_dir,
        newer,
        "model-newer",
        "newer prompt",
        &cwd_str,
    );
    stamp_age(&sessions_dir.join(older).join("log.jsonl"), 100);
    stamp_age(&sessions_dir.join(newer).join("log.jsonl"), 10);
    let _newer_lock = common::hold_session_lock(&sessions_dir.join(newer).join("session.lock"));
    let _older_lock = common::hold_session_lock(&sessions_dir.join(older).join("session.lock"));

    let mut s = PtySession::launch_with_sessions_dir(
        None,
        None,
        None,
        Some(ws.clone()),
        &["--continue".to_string()],
        sessions_dir.clone(),
    );
    assert!(
        s.wait_for_plain("no free session", RENDER_TIMEOUT),
        "--continue should refuse instead of starting an empty session:\n{}",
        s.output()
    );
    for sid in [older, newer] {
        let hint = format!("houyi --resume {sid} --fork-session");
        assert!(
            s.wait_for_plain(&hint, RENDER_TIMEOUT),
            "the refusal should name this session's fork command:\n{}",
            s.output()
        );
    }
    assert!(
        s.wait_for_exit(RENDER_TIMEOUT),
        "the refused launch should exit:\n{}",
        s.output()
    );
    remove_dir_all(ws).ok();
}

/// Resuming a session whose recorded cwd no longer exists does not crash:
/// the binary runs in its launch cwd, the descriptor cwd is informational,
/// /status shows the stale recorded path. Guards the stale-descriptor-cwd
/// boundary.
#[test]
#[ignore]
fn test_sid_deleted_cwd_degrades() {
    let sessions_dir = fresh_temp_dir("sessions-cwd-deleted");
    let sid = "66666666-6666-6666-6666-666666666666";
    let event = SessionLogEntry {
        id: EventId::new(),
        session: SessionId::from_display_string(sid).unwrap(),
        ts: 0,
        prev_hash: None,
        event: SessionEvent::UserInput {
            text: "cwd-deleted session".into(),
        },
    };
    let dir = sessions_dir.join(sid);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("log.jsonl"),
        format!("{}\n", serde_json::to_string(&event).unwrap()),
    )
    .unwrap();
    let meta = serde_json::json!({
        "name": null,
        "name_source": "auto",
        "cwd": "/nonexistent/houyi-deleted-cwd-test-path",
        "model": "cwd-deleted-model",
        "provenance": {"kind": "fresh"},
        "version": "test",
        "created_at": 1000,
    });
    fs::write(
        dir.join("session.json"),
        serde_json::to_string_pretty(&meta).unwrap(),
    )
    .unwrap();
    let mut s = PtySession::launch_with_sessions_dir(
        None,
        None,
        None,
        None,
        &["--resume".to_string(), sid.to_string()],
        sessions_dir.clone(),
    );
    assert!(
        s.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "working screen despite the deleted recorded cwd:\n{}",
        s.output()
    );
    run_slash_command(&mut s, "status");
    assert!(
        s.wait_for_plain("cwd:", RENDER_TIMEOUT),
        "/status should render cwd despite the stale path:\n{}",
        s.output()
    );
}

/// /status inline rename journey: open the Status tab, press e to edit the
/// session name, type a new name, Enter commits. The pane re-renders with
/// the new name + the terminal tab title OSC 0/2 bytes land in the output
/// stream. Houyi makes the session name inline-editable + syncs the tab title
/// (rather than a rename command). Covers the full interaction
/// sequence (trigger, edit, commit, observable rename + OSC).
#[test]
#[ignore]
fn test_status_rename_emits_title() {
    let sessions_dir = fresh_temp_dir("sessions-inline-rename");
    let sid = "88888888-8888-8888-8888-888888888888";
    common::seed_session_on_disk(&sessions_dir, sid, "rename-model", "rename seed prompt");
    let mut s = PtySession::launch_with_sessions_dir(
        None,
        None,
        None,
        None,
        &["--resume".to_string(), sid.to_string()],
        sessions_dir.clone(),
    );
    assert!(
        s.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "working screen after resume:\n{}",
        s.output()
    );
    run_slash_command(&mut s, "status");
    assert!(
        s.wait_for_screen("Session name", RENDER_TIMEOUT),
        "/status should render the name row:\n{}",
        s.screen().contents()
    );
    s.send_key(&Key::Char('e'));
    assert!(
        s.wait_for_plain("(Enter save · Esc cancel)", RENDER_TIMEOUT),
        "the rename editor should open:\n{}",
        s.output_plain()
    );
    // The editor opens empty (no prefill) so an Auto name is not silently
    // promoted to User by seeding the buffer; typing lands on a clean buffer.
    s.send_str("shiny-new-name");
    s.send_key(&Key::Enter);
    // Assert the reconstructed screen, not the byte stream: the renderer
    // redraws only the cells a frame changes, so the raw stream can hold a
    // partial copy of the row while the screen holds the whole name.
    assert!(
        s.wait_for_screen("shiny-new-name", RENDER_TIMEOUT),
        "pane should show the renamed session:\n{}",
        s.screen().contents()
    );
    // The tab title follows a status snapshot, which the host polls once a
    // second, so the rename paints long before the OSC arrives. Poll for the
    // bytes; checking the stream once would lose that window under load.
    assert!(
        s.wait_for("\u{1b}]0;shiny-new-name", RENDER_TIMEOUT),
        "OSC 0/2 tab title bytes should be in the stream:\n{}",
        s.output()
    );
    // The descriptor write is durable and may still be in flight while the
    // pane already shows the name, so poll for the persisted form too.
    let descriptor_path = sessions_dir.join(sid).join("session.json");
    let deadline = Instant::now() + RENDER_TIMEOUT;
    let mut descriptor = String::new();
    while Instant::now() < deadline {
        descriptor = fs::read_to_string(&descriptor_path).unwrap_or_default();
        if descriptor.contains("\"name\": \"shiny-new-name\"") && descriptor.contains("\"user\"") {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        descriptor.contains("\"name\": \"shiny-new-name\""),
        "descriptor should persist the renamed name:\n{descriptor}"
    );
    assert!(
        descriptor.contains("\"user\""),
        "descriptor should mark name_source=user:\n{descriptor}"
    );
}

/// User journey: a /resume while a run is in flight defers the swap. A run is in flight
/// (a delayed response), the user opens the /resume picker + Enter, the
/// swap is enqueued as a Command with a "will switch" hint + does NOT happen
/// while the run is live. After the run ends, the idle drain dispatches the
/// Command + swaps in-process, loading the target session's history.
#[test]
#[ignore]
fn test_during_run_defers_swap() {
    let sessions_dir = fresh_temp_dir("sessions-defer-resume");
    let sid_b = "99999999-9999-9999-9999-999999999999";
    // Three turns, so the swap is proven by the middle row. The picker paints
    // the newest prompt as its title and the opening one as the selected
    // row's topic, and the middle turn is in neither: only the loaded history
    // renders it.
    let history = [
        SessionEvent::UserInput {
            text: "session B prompt".into(),
        },
        SessionEvent::UserInput {
            text: "session B tail".into(),
        },
        SessionEvent::UserInput {
            text: "session B last".into(),
        },
    ];
    common::seed_session_events(&sessions_dir, sid_b, "defer-model-b", &history);
    // One-turn script: "done" text. The stub delay keeps the run in flight
    // while the picker round trip lands; a longer window only adds wall time.
    let script = r#"[ [{"type":"Text","text":"done"}] ]"#;
    let mut s = PtySession::launch_with_sessions_dir(
        Some(script.to_string()),
        Some(900),
        None,
        None,
        &[],
        sessions_dir.clone(),
    );
    assert!(
        s.wait_for("sign in to houyicoder", RENDER_TIMEOUT),
        "login screen"
    );
    s.send_key(&Key::Char('3'));
    assert!(
        s.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "working screen"
    );
    // Start a run (in flight during the scripted stub delay).
    s.send_str("run it");
    s.send_key(&Key::Enter);
    // Resume while the run is live: open the picker.
    run_slash_command(&mut s, "resume");
    assert!(
        s.wait_for_screen("Resume a session", RENDER_TIMEOUT),
        "picker should open while the run is live:\n{}",
        s.screen().contents()
    );
    // The second turn is the marker the swap is proven by, and no picker row
    // carries it: only the loaded history does.
    assert!(
        !s.screen().contents().contains("session B tail"),
        "the picker must not paint the history marker:\n{}",
        s.screen().contents()
    );
    // Enter (busy) -> enqueue Command + "will switch" hint.
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for_plain("resume: will", RENDER_TIMEOUT),
        "defer message should appear:\n{}",
        s.output_plain()
    );
    // The session is NOT switched yet: the status bar still names this
    // session's model, and no frame so far has drawn session B's. The picker
    // row is not a usable marker here -- it titles the row with B's prompt.
    assert!(
        !s.output_plain().contains("defer-model-b"),
        "session must NOT switch while the run is live:\n{}",
        s.output_plain()
    );
    // The run ends after the delay -> "done" -> idle_drain dispatches the
    // Command -> swap. Clear the picker frames first, so what follows can
    // only come from the swapped-in session.
    s.clear_output();
    assert!(
        s.wait_for_plain("defer-model-b", RENDER_TIMEOUT),
        "swap should restore session B's model after the run ends:\n{}",
        s.output_plain()
    );
    // The screen keeps its cells past clear_output, which is why the wait
    // below names a row the picker never painted, not the prompt it titles
    // with. A redrawn frame writes only the cells that changed, so the bytes
    // alone can drop a letter the screen still holds from the earlier frame.
    assert!(
        s.wait_for_screen("session B tail", RENDER_TIMEOUT),
        "swap should load session B's history after the run ends:\n{}",
        s.screen().contents()
    );
}

/// Cross-swap auto-send: a message enqueued after a deferred /resume
/// Command is carried across the swap into the new session + auto-sends as
/// the new session's first turn (a queue
/// processor fires the next item the instant the run goes idle). The barrier
/// keeps it host-side (no InjectUser to the old server), so it survives the
/// swap; swap_session demotes it to ParkedMessage (the new runner's server
/// queue is empty), then the idle drain spawns it fresh in the new session.
#[test]
#[ignore]
fn test_swap_carries_queued_message() {
    let sessions_dir = fresh_temp_dir("sessions-cross-swap");
    let sid_b = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
    common::seed_session_on_disk(
        &sessions_dir,
        sid_b,
        "cross-swap-model-b",
        "session B prompt",
    );
    let script = r#"[ [{"type":"Text","text":"done"}], [{"type":"Text","text":"done b"}] ]"#;
    let mut s = PtySession::launch_with_sessions_dir(
        Some(script.to_string()),
        Some(900),
        None,
        None,
        &[],
        sessions_dir.clone(),
    );
    assert!(s.wait_for("sign in to houyicoder", RENDER_TIMEOUT), "login");
    s.send_key(&Key::Char('3'));
    assert!(
        s.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "working screen"
    );
    // Start a run (in flight during the scripted stub delay).
    s.send_str("run it");
    s.send_key(&Key::Enter);
    // Enqueue the resume Command first (the barrier).
    run_slash_command(&mut s, "resume");
    assert!(
        s.wait_for_screen("Resume a session", RENDER_TIMEOUT),
        "picker opens:\n{}",
        s.screen().contents()
    );
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for_plain("resume: will", RENDER_TIMEOUT),
        "defer hint:\n{}",
        s.output_plain()
    );
    // Now enqueue a message AFTER the barrier: host-side only, no InjectUser.
    s.send_str("task after swap");
    s.send_key(&Key::Enter);
    // The run ends -> idle drain: the queued /resume Command swaps to the
    // new session, carrying the queued message across (demoted to
    // ParkedMessage). The carried message then
    // auto-drains + sends as the new session's first turn (the
    // queue processor fires it on idle). The status bar's model name is the
    // stable swap-landed signal (the seeded prompt + the auto-sent message
    // render in the same frame, so the prompt text is not a reliable
    // contiguous marker).
    assert!(
        s.wait_for_plain("cross-swap-model-b", RENDER_TIMEOUT),
        "swap lands + shows the new session's model:\n{}",
        s.output_plain()
    );
    // The carried message auto-sent in the new session (User echo), not
    // parked for recall.
    assert!(
        s.wait_for_plain("task after swap", RENDER_TIMEOUT),
        "carried message auto-sends in the new session after the swap:\n{}",
        s.output_plain()
    );
}

/// Multiple messages enqueued after a deferred /resume Command are all
/// carried across the swap + auto-send in the new session, FIFO (queue
/// semantics). Both stay host-side (barrier blocks InjectUser),
/// survive the swap, + send sequentially as the prior run completes.
#[test]
#[ignore]
fn test_swap_carries_multi_msgs() {
    let sessions_dir = fresh_temp_dir("sessions-multi-carry");
    let sid_b = "cccccccc-cccc-cccc-cccc-cccccccccccc";
    common::seed_session_on_disk(
        &sessions_dir,
        sid_b,
        "multi-carry-model-b",
        "session B prompt",
    );
    let script = r#"[ [{"type":"Text","text":"done"}], [{"type":"Text","text":"done b1"}], [{"type":"Text","text":"done b2"}] ]"#;
    let mut s = PtySession::launch_with_sessions_dir(
        Some(script.to_string()),
        Some(900),
        None,
        None,
        &[],
        sessions_dir.clone(),
    );
    assert!(s.wait_for("sign in to houyicoder", RENDER_TIMEOUT), "login");
    s.send_key(&Key::Char('3'));
    assert!(
        s.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "working"
    );
    s.send_str("run it");
    s.send_key(&Key::Enter);
    // /resume first (the barrier) via the picker, then two messages after it.
    run_slash_command(&mut s, "resume");
    assert!(
        s.wait_for_screen("Resume a session", RENDER_TIMEOUT),
        "picker opens:\n{}",
        s.screen().contents()
    );
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for_plain("resume: will", RENDER_TIMEOUT),
        "defer hint:\n{}",
        s.output_plain()
    );
    s.send_str("first queued");
    s.send_key(&Key::Enter);
    s.send_str("second queued");
    s.send_key(&Key::Enter);
    // run1 ends -> drain /resume Command -> swap (carries [first queued,
    // second queued], both demoted to ParkedMessage). Each auto-sends in
    // the new session FIFO as the prior run completes. The status bar's
    // model name is the stable swap-landed signal.
    assert!(
        s.wait_for_plain("multi-carry-model-b", RENDER_TIMEOUT),
        "swap lands + shows the new session's model:\n{}",
        s.output_plain()
    );
    assert!(
        s.wait_for_plain("first queued", RENDER_TIMEOUT),
        "first carried message auto-sends in the new session:\n{}",
        s.output_plain()
    );
    assert!(
        s.wait_for_plain("second queued", RENDER_TIMEOUT),
        "second carried message auto-sends after the first completes:\n{}",
        s.output_plain()
    );
}

/// /clear as a barrier: a /clear mid-run enqueues as a Command, a message
/// typed after it stays host-side (barrier blocks InjectUser). After the run
/// ends, /clear drains + clears the transcript; the queued message then runs
/// on the cleared session. Verifies /clear's barrier + the post-clear run.
#[test]
#[ignore]
fn test_clear_barrier_msg_runs() {
    let script = r#"[ [{"type":"Text","text":"done"}] ]"#;
    let mut s = PtySession::launch_with_sessions_dir(
        Some(script.to_string()),
        Some(500),
        None,
        None,
        &[],
        fresh_temp_dir("sessions-clear-barrier"),
    );
    assert!(s.wait_for("sign in to houyicoder", RENDER_TIMEOUT), "login");
    s.send_key(&Key::Char('3'));
    assert!(
        s.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "working"
    );
    s.send_str("run it");
    s.send_key(&Key::Enter);
    // /clear mid-run (barrier), then a message after it.
    run_slash_command(&mut s, "clear");
    assert!(
        s.wait_for_plain("will run when the run finishes", RENDER_TIMEOUT),
        "/clear defer hint:\n{}",
        s.output_plain()
    );
    s.send_str("task after clear");
    s.send_key(&Key::Enter);
    // run1 ends -> drain /clear (clears transcript, emits the archive notice)
    // -> drain the message -> run2 -> "done". run1's "done" is rebuilt + cleared
    // in the same event-loop iteration (no render between), so assert the
    // archive notice (/clear drained) + run2's "done" instead. The notice is
    // read from the screen, not the output stream: the renderer emits only the
    // cells that changed, so a notice landing on a row that already carried
    // some of its letters never appears whole in the stream.
    assert!(
        s.wait_for_screen("new session started", RENDER_TIMEOUT),
        "/clear drained + cleared the transcript:\n{}",
        s.output_plain()
    );
    s.clear_output();
    assert!(
        s.wait_for_plain("done", RENDER_TIMEOUT),
        "queued message ran on the cleared session (run2 done):\n{}",
        s.output_plain()
    );
}

/// Launch-resume chained with in-session resume: --resume <sid-A> enters A
/// (A's history loads), then an in-session /resume swaps to B in-process.
/// This pins the chain the single-leg tests do not cover: the launch-resume
/// path (a fresh binary resuming an on-disk sid) followed by the in-session
/// swap path (the event loop's try_swap_session) on the same process. A
/// regression here would mean the launch-resumed session's resume_builder is
/// mis-wired, so a second /resume after launch no-ops or crashes.
#[test]
#[ignore]
fn test_launch_resume_chains_session() {
    let sessions_dir = fresh_temp_dir("sessions-chain-resume");
    let sid_a = "dddddddd-dddd-dddd-dddd-dddddddddddd";
    let sid_b = "eeeeeeee-eeee-eeee-eeee-eeeeeeeeeeee";
    common::seed_session_on_disk(&sessions_dir, sid_a, "chain-model-a", "session A prompt");
    common::seed_session_on_disk(&sessions_dir, sid_b, "chain-model-b", "session B prompt");
    // Launch resuming A: the binary reopens A's history on the working screen.
    let mut s = PtySession::launch_with_sessions_dir(
        None,
        None,
        None,
        None,
        &["--resume".to_string(), sid_a.to_string()],
        sessions_dir.clone(),
    );
    assert!(
        s.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "working screen after launch --resume A:\n{}",
        s.output()
    );
    // The replayed prompt is a rendered line, and a cell-diff repaint can
    // splice the words in the byte stream.
    assert!(
        s.wait_for_screen("session A prompt", RENDER_TIMEOUT),
        "A's history must load on launch-resume:\n{}",
        s.screen().contents()
    );
    // In-session /resume: open the picker, Enter picks B (the only other
    // session), the event loop swaps in-process (no quit, no re-exec).
    run_slash_command(&mut s, "resume");
    assert!(
        s.wait_for_screen("Resume a session", RENDER_TIMEOUT),
        "picker opens from the launch-resumed session:\n{}",
        s.screen().contents()
    );
    s.send_key(&Key::Enter);
    // The picker row already carried B's title, so clear those frames; the
    // model + prompt below can then only come from the swapped-in session.
    s.clear_output();
    assert!(
        s.wait_for_plain("chain-model-b", RENDER_TIMEOUT),
        "in-session /resume must restore B's model:\n{}",
        s.output_plain()
    );
    // The picker row already put this prompt on screen, and the screen keeps
    // its cells past clear_output, so the byte stream is the only witness.
    assert!(
        s.wait_for_compact("sessionBprompt", RENDER_TIMEOUT),
        "in-session /resume must swap from A to B (launch-resume + swap chained):\n{}",
        s.output_plain()
    );
}

/// After --resume <sid>, a continued turn + /export produces an export file
/// carrying BOTH the resumed history and the continued turn. The live-export
/// tests cover export-from-live (a fresh run's first turn exported mid-run);
/// this covers the re-export round-trip: resume loads seeded history, a new
/// turn appends to the same sid log, then /export must serialize the full
/// trajectory. Asserts on the export FILE content (not the screen) so it is
/// falsifiable end-to-end on the durable trajectory.
#[test]
#[ignore]
fn test_reexport_resume_keeps_history() {
    let sessions_dir = fresh_temp_dir("sessions-reexport");
    let sid = "13131313-1313-1313-1313-131313131313";
    common::seed_session_on_disk(
        &sessions_dir,
        sid,
        "reexport-model",
        "seeded roundtrip prompt",
    );
    let mut s = PtySession::launch_with_sessions_dir(
        None,
        None,
        None,
        None,
        &["--resume".to_string(), sid.to_string()],
        sessions_dir.clone(),
    );
    assert!(
        s.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "working screen after sid resume:\n{}",
        s.output()
    );
    // Append a continued turn; the no-api-key stub still flushes the user
    // input to the durable log (the assistant reply is irrelevant here).
    s.send_str("continuation after resume");
    s.send_key(&Key::Enter);
    // /export reads the durable log, so the continuation landing there is the
    // latch for the flush.
    assert!(
        common::wait_file_contains(
            &sessions_dir.join(sid).join("log.jsonl"),
            "continuation after resume",
            RENDER_TIMEOUT
        ),
        "the continued turn should reach the durable log:\n{}",
        s.output()
    );
    let export_path = sessions_dir.join("reexport.json");
    run_slash_command(&mut s, &format!("export {}", export_path.display()));
    assert!(
        s.wait_for("export: wrote", RENDER_TIMEOUT),
        "export should report a write after resume + continuation:\n{}",
        s.output()
    );
    let exported = fs::read_to_string(&export_path)
        .unwrap_or_else(|_| panic!("export file missing at {export_path:?}"));
    assert!(
        exported.contains("seeded roundtrip prompt"),
        "export must carry the resumed (seeded) history:\n{exported}"
    );
    assert!(
        exported.contains("continuation after resume"),
        "export must carry the continued turn:\n{exported}"
    );
}

/// A session switch reuses the provider resolved at startup, not re-resolving
/// the key source. The helper appends one line to a marker file per spawn;
/// startup resolve writes one line, and an in-TUI /resume must not write a
/// second. The helper prints nothing to stdout on purpose so resolution falls
/// through to env and then stub -- no network -- only the spawn is observed.
/// Broken, a per-switch re-resolve, doubles the marker. The sync point is the
/// resumed history rendering: count the marker only after the switch landed.
#[test]
#[ignore]
fn test_resume_reuses_provider() {
    let home = fresh_temp_dir("home-resolve-once");
    let sessions_dir = fresh_temp_dir("sessions-resolve-once");
    let marker = env::temp_dir().join(format!(
        "houyi-resolve-marker-{}-{}",
        process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    drop(fs::remove_file(&marker));
    // Built with serde_json so a temp path's backslashes on Windows escape
    // rather than break the JSON string.
    let settings = serde_json::json!({
        "apiKeyHelper": format!("echo >> {}", marker.display())
    });
    fs::create_dir_all(home.join(".houyicoder")).unwrap();
    fs::write(
        home.join(".houyicoder").join("settings.json"),
        settings.to_string(),
    )
    .unwrap();
    let sid_b = "44444444-4444-4444-4444-444444444444";
    common::seed_session_on_disk(&sessions_dir, sid_b, "reuse-model", "seeded prompt");
    let mut s = PtySession::launch_with_sessions_dir(
        None,
        None,
        Some(home.clone()),
        None,
        &[],
        sessions_dir.clone(),
    );
    assert!(
        s.wait_for("sign in to houyicoder", RENDER_TIMEOUT),
        "login screen"
    );
    s.send_key(&Key::Char('3'));
    assert!(
        s.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "working screen"
    );
    // Startup resolve ran once.
    let after_start = fs::read_to_string(&marker).unwrap_or_default();
    assert_eq!(
        after_start.lines().count(),
        1,
        "startup resolved the helper once:\n{after_start}"
    );
    // In-TUI /resume to the seeded session -- a switch, not a fresh launch.
    run_slash_command(&mut s, &format!("resume {sid_b}"));
    // Sync point: the resumed history renders, so the switch landed. A broken
    // per-switch re-resolve would have run the helper again before this.
    assert!(
        s.wait_for("seeded prompt", RENDER_TIMEOUT),
        "switch must land and show resumed history:\n{}",
        s.output()
    );
    let after_switch = fs::read_to_string(&marker).unwrap_or_default();
    assert_eq!(
        after_switch.lines().count(),
        1,
        "session switch must reuse the startup-resolved provider:\n{after_switch}"
    );
    drop(fs::remove_file(&marker));
    drop(fs::remove_dir_all(&home));
    drop(fs::remove_dir_all(&sessions_dir));
}

/// User journey: a wide character typed into the resume picker reaches the
/// filter. The terminal hands the key over as UTF-8 bytes, and the header
/// can only echo the character back once those bytes decode to one.
#[test]
#[ignore]
fn test_picker_echoes_wide_query() {
    let sessions_dir = fresh_temp_dir("sessions-picker-wide");
    common::seed_session_on_disk(
        &sessions_dir,
        "11111111-1111-1111-1111-111111111111",
        "stub-model-a",
        "整理缓存表",
    );
    let mut s =
        PtySession::launch_with_sessions_dir(None, None, None, None, &[], sessions_dir.clone());
    assert!(
        s.wait_for("sign in to houyicoder", RENDER_TIMEOUT),
        "login screen"
    );
    s.send_key(&Key::Char('3'));
    assert!(
        s.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "working screen"
    );
    run_slash_command(&mut s, "resume");
    assert!(
        s.wait_for_screen("Resume a session", RENDER_TIMEOUT),
        "picker opens:\n{}",
        s.screen().contents()
    );
    s.send_str("整");
    assert!(
        s.wait_for_screen("filter: 整", RENDER_TIMEOUT),
        "the typed wide character reaches the filter:\n{}",
        s.screen().contents()
    );
}
