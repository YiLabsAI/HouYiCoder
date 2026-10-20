//! Resume PTY journey tests that drive the real binary: the sid-resume, lock
//! release (clean exit + crash), status provenance, stale-cwd-degraded, and
//! history-load journeys. Helpers are shared via the common module.

#![allow(clippy::unwrap_in_result)]

use std::env;
use std::fs;
use std::path::Path;
use std::process;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use houyicoder_core::{EventId, SessionEvent, SessionId, SessionLogEntry};

use crate::common::{
    self, Key, PtySession, RENDER_TIMEOUT, fresh_temp_dir, parse_scroll_status, run_slash_command,
    seed_large_session_on_disk,
};
use std::thread::sleep;

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
        s.wait_for_screen("input tokens:", RENDER_TIMEOUT),
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
        !s2.output().contains("held by another"),
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
        s.wait_for_plain("provenance:", RENDER_TIMEOUT),
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
        !s2.output().contains("held by another"),
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
        s.wait_for_plain("Session name", RENDER_TIMEOUT),
        "/status should render the name row:\n{}",
        s.output()
    );
    s.send_key(&Key::Char('e'));
    thread::sleep(Duration::from_millis(200));
    // The editor opens empty (no prefill) so an Auto name is not silently
    // promoted to User by seeding the buffer; typing lands on a clean buffer.
    s.send_str("shiny-new-name");
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for_plain("shiny-new-name", RENDER_TIMEOUT),
        "pane should show the renamed session:\n{}",
        s.output_plain()
    );
    assert!(
        s.output().contains("\u{1b}]0;shiny-new-name"),
        "OSC 0/2 tab title bytes should be in the stream:\n{}",
        s.output()
    );
    let descriptor = fs::read_to_string(sessions_dir.join(sid).join("session.json"))
        .unwrap_or_else(|_| String::new());
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
    common::seed_session_on_disk(&sessions_dir, sid_b, "defer-model-b", "session B prompt");
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
        s.wait_for_plain("Resume a session", RENDER_TIMEOUT),
        "picker should open while the run is live:\n{}",
        s.output_plain()
    );
    // Enter (busy) -> enqueue Command + "will switch" hint.
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for_plain("resume: will", RENDER_TIMEOUT),
        "defer message should appear:\n{}",
        s.output_plain()
    );
    // The session is NOT switched yet (session B's prompt absent).
    assert!(
        !s.output_plain().contains("session B prompt"),
        "session must NOT switch while the run is live:\n{}",
        s.output_plain()
    );
    // The run ends after the delay -> "done" -> idle_drain dispatches the
    // Command -> swap. The swap loads session B's history.
    assert!(
        s.wait_for_plain("session B prompt", RENDER_TIMEOUT),
        "swap should load session B after the run ends:\n{}",
        s.output_plain()
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
        s.wait_for_plain("Resume a session", RENDER_TIMEOUT),
        "picker opens:\n{}",
        s.output_plain()
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
        s.wait_for_plain("Resume a session", RENDER_TIMEOUT),
        "picker opens:\n{}",
        s.output_plain()
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
    assert!(
        s.wait_for("session A prompt", RENDER_TIMEOUT),
        "A's history must load on launch-resume:\n{}",
        s.output()
    );
    // In-session /resume: open the picker, Enter picks B (the only other
    // session), the event loop swaps in-process (no quit, no re-exec).
    run_slash_command(&mut s, "resume");
    assert!(
        s.wait_for_plain("Resume a session", RENDER_TIMEOUT),
        "picker opens from the launch-resumed session:\n{}",
        s.output_plain()
    );
    s.send_key(&Key::Enter);
    // The swap loads B's history in-process; A's prompt is replaced by B's.
    assert!(
        s.wait_for_plain("session B prompt", RENDER_TIMEOUT),
        "in-session /resume must swap from A to B (launch-resume + swap chained):\n{}",
        s.output_plain()
    );
}

/// After --resume <sid>, the /trajectory pane lists the seeded turn: the
/// header reads "1 turn" (not "0 turns") + the turn row renders the seeded
/// user input. Discriminates from the transcript-render test: the output is
/// cleared before opening the trajectory pane, so the seeded text can only
/// come from the trajectory turn-list, not the transcript scrollback.
#[test]
#[ignore]
fn test_trajectory_shows_resumed_events() {
    let sessions_dir = fresh_temp_dir("sessions-traj-resume");
    let sid = "12121212-1212-1212-1212-121212121212";
    common::seed_session_on_disk(&sessions_dir, sid, "traj-model", "seeded trajectory prompt");
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
    // Isolate the trajectory pane: clear scrollback so the seeded text below
    // can only come from the trajectory turn-list, not the transcript.
    s.clear_output();
    run_slash_command(&mut s, "trajectory");
    // The singular is part of the match, not a separate assertion: a header
    // that dropped the count into a fixed plural phrase renders "1 turns", and
    // the word boundary here rejects it.
    assert!(
        s.wait_for_plain("1 turn ", RENDER_TIMEOUT),
        "trajectory header must report one seeded turn in the singular (not 0, not 1 turns):\n{}",
        s.output_plain()
    );
    assert!(
        s.wait_for_compact("seededtrajectoryprompt", RENDER_TIMEOUT),
        "trajectory turn row must render the seeded user input:\n{}",
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

/// The trajectory list does not wrap. Up from the oldest turn stays there and
/// Down from the newest stays there: a session can hold hundreds of turns, and
/// a wrap would jump the whole history on one keystroke. Driven through the real
/// terminal so the pane's own key routing is what is under test.
#[test]
#[ignore]
fn test_trajectory_no_wrap_nav() {
    let sessions_dir = fresh_temp_dir("sessions-traj-nowrap");
    let sid = "14141414-1414-1414-1414-141414141414";
    common::seed_session_turns_on_disk(
        &sessions_dir,
        sid,
        "traj-model",
        &["first prompt", "second prompt", "third prompt"],
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
    s.clear_output();
    run_slash_command(&mut s, "trajectory");
    assert!(
        s.wait_for_plain("3 turns", RENDER_TIMEOUT),
        "trajectory header must report the three seeded turns:\n{}",
        s.output_plain()
    );
    // Home selects the oldest turn, and Up there must not wrap to the newest.
    s.send_key(&Key::Home);
    sleep(Duration::from_millis(200));
    assert!(
        s.screen().contents().contains("▸ T1"),
        "Home selects the oldest turn:\n{}",
        s.screen().contents()
    );
    s.send_key(&Key::Up);
    sleep(Duration::from_millis(200));
    let screen = s.screen().contents();
    assert!(
        screen.contains("▸ T1"),
        "Up at the oldest turn stays there instead of wrapping to the newest:\n{screen}"
    );
    // End selects the newest, and Down there must not wrap to the oldest.
    s.send_key(&Key::End);
    sleep(Duration::from_millis(200));
    assert!(
        s.screen().contents().contains("▸ T3"),
        "End selects the newest turn:\n{}",
        s.screen().contents()
    );
    s.send_key(&Key::Down);
    sleep(Duration::from_millis(200));
    let screen = s.screen().contents();
    assert!(
        screen.contains("▸ T3"),
        "Down at the newest turn stays there instead of wrapping to the oldest:\n{screen}"
    );
    drop(s);
}

/// Walking to the top of the loaded window reads the older page behind it: the
/// window widens rather than replacing the tail, and the header stops saying
/// turns are not loaded. Driven through the real terminal, so the pane's own
/// key routing and the worker read are what is under test.
#[test]
#[ignore]
fn test_trajectory_older_loads_page() {
    let sessions_dir = fresh_temp_dir("sessions-traj-older");
    let sid = "16161616-1616-1616-1616-161616161616";
    let prompts: Vec<String> = (0..150).map(|i| format!("older prompt {i}")).collect();
    let refs: Vec<&str> = prompts.iter().map(String::as_str).collect();
    common::seed_session_turns_on_disk(&sessions_dir, sid, "traj-model", &refs);
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
    s.clear_output();
    run_slash_command(&mut s, "trajectory");
    // The first page holds the newest hundred turns, so fifty are not loaded.
    assert!(
        s.wait_for_screen("150 turns (50 older not loaded)", RENDER_TIMEOUT),
        "the first page reports the turns it has not read:\n{}",
        s.screen().contents()
    );
    assert!(
        s.screen().contents().contains("older prompt 149"),
        "and it holds the newest turn:\n{}",
        s.screen().contents()
    );
    // Walk to the top of the window, where Up reads the older page.
    for _ in 0..110 {
        s.send_key(&Key::Up);
    }
    let loaded = (0..80).any(|_| {
        sleep(Duration::from_millis(100));
        !s.screen().contents().contains("older not loaded")
    });
    assert!(
        loaded,
        "the older page lands and the header stops hiding turns:\n{}",
        s.screen().contents()
    );
    // The selection keeps the turn it was on across the load, so the window
    // grew above the user rather than pulling the oldest turns into view.
    // Walking on to the top of the widened window reaches the session's first
    // turn.
    let showing_oldest = (0..40).any(|_| {
        for _ in 0..5 {
            s.send_key(&Key::Up);
        }
        sleep(Duration::from_millis(100));
        s.screen().contents().contains("older prompt 0")
    });
    assert!(
        showing_oldest,
        "and the walk reaches the session's oldest turn:\n{}",
        s.screen().contents()
    );
    drop(s);
}

/// Leaving the turn detail returns to the turn the drill opened, and Up at the
/// record list stays on the record list instead of loading older history behind
/// the drill. Driven through the real terminal, so the pane's own key routing
/// and its focus are what is under test.
#[test]
#[ignore]
fn test_trajectory_esc_restores_turn() {
    let sessions_dir = fresh_temp_dir("sessions-traj-esc");
    let sid = "18181818-1818-1818-1818-181818181818";
    common::seed_session_turns_on_disk(
        &sessions_dir,
        sid,
        "traj-model",
        &["first prompt", "second prompt", "third prompt"],
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
    s.clear_output();
    run_slash_command(&mut s, "trajectory");
    assert!(
        s.wait_for_plain("3 turns", RENDER_TIMEOUT),
        "trajectory header must report the three seeded turns:\n{}",
        s.output_plain()
    );
    // The pane opens on the tail with the newest turn selected, which is also
    // how the test knows the first page has landed.
    assert!(
        s.wait_for("▸ T3", RENDER_TIMEOUT),
        "the pane opens on the newest turn:\n{}",
        s.screen().contents()
    );

    // Walk up to the second turn and open it.
    s.send_key(&Key::Up);
    sleep(Duration::from_millis(200));
    assert!(
        s.screen().contents().contains("▸ T2"),
        "Up selects the second turn:\n{}",
        s.screen().contents()
    );
    s.send_key(&Key::Enter);
    sleep(Duration::from_millis(200));
    let detail = s.screen().contents();
    assert!(
        detail.contains("T2"),
        "Enter opens the selected turn:\n{detail}"
    );

    // Up at the record list must not move the turn list behind the drill.
    s.send_key(&Key::Up);
    sleep(Duration::from_millis(200));
    let detail = s.screen().contents();
    assert!(
        detail.contains("T2") && !detail.contains("older not loaded"),
        "Up at the record list stays on the record list:\n{detail}"
    );

    // Esc returns to the list with the turn the drill opened still selected.
    s.send_key(&Key::Esc);
    sleep(Duration::from_millis(200));
    let back = s.screen().contents();
    assert!(
        back.contains("▸ T2"),
        "Esc returns to the turn the drill opened:\n{back}"
    );
    drop(s);
}

/// The lines the seeded reply is made of. The record body carries them one per
/// row, which is how a test reads where the body is scrolled to.
const BODY_LINE_PREFIX: &str = "paged-line-";

/// The key hints each level of the drill states, which is how a test tells
/// which level the pane has drawn without reading the pane's own state.
const TURN_LIST_FOOTER: &str = "Esc to close";
const RECORD_LIST_FOOTER: &str = "Enter to open · Esc to back";
const RECORD_BODY_FOOTER: &str = "Up/Down to scroll · Esc to back";

/// The record-list row the cursor is on, or None while no row carries it.
fn selected_row(screen: &str) -> Option<&str> {
    screen.lines().find(|row| row.contains('▸'))
}

/// The body lines the pane is showing, each as the screen row it sits on and
/// its number, read off the drawn screen: which line the body starts at, and
/// how far what the reader was looking at moved, is what a scroll either did
/// or did not do.
fn body_rows(screen: &str) -> Vec<(usize, usize)> {
    screen
        .lines()
        .enumerate()
        .filter_map(|(row, text)| {
            let at = text.find(BODY_LINE_PREFIX)?;
            let number = text[at + BODY_LINE_PREFIX.len()..]
                .split_whitespace()
                .next()?
                .parse::<usize>()
                .ok()?;
            Some((row, number))
        })
        .collect()
}

/// The first body line the pane is showing.
fn first_body_line(screen: &str) -> Option<usize> {
    body_rows(screen)
        .into_iter()
        .map(|(_, number)| number)
        .min()
}

/// Poll the drawn screen until the predicate holds. A key's effect arrives with
/// the next frame, so waiting on the effect is what keeps a sequence of keys
/// from running ahead of the level each one acts on.
fn wait_until(s: &mut PtySession, holds: impl Fn(&str) -> bool) -> bool {
    for _ in 0..60 {
        if holds(&s.screen().contents()) {
            return true;
        }
        sleep(Duration::from_millis(50));
    }
    false
}

/// Drill from the turn list to the model record's body: Enter opens the turn,
/// Down walks the cursor down to the model record, Enter opens that record.
/// Each step waits on the drawn screen, so a key cannot act on a level the
/// frame behind it has not drawn yet.
fn open_model_record(s: &mut PtySession) {
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for_screen(RECORD_LIST_FOOTER, RENDER_TIMEOUT),
        "Enter opens the turn's record list:\n{}",
        s.screen().contents()
    );
    // The list opens on the turn's first record. The model call is the last
    // one the turn holds, behind its context line and its tool call.
    let mut on_model = false;
    for _ in 0..3 {
        s.send_key(&Key::Down);
        if wait_until(s, |screen| {
            selected_row(screen).is_some_and(|row| row.contains("model"))
        }) {
            on_model = true;
            break;
        }
    }
    assert!(
        on_model,
        "Down walks the cursor to the model record:\n{}",
        s.screen().contents()
    );
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for_screen(RECORD_BODY_FOOTER, RENDER_TIMEOUT),
        "Enter opens the record the cursor is on:\n{}",
        s.screen().contents()
    );
    assert!(
        s.screen().contents().contains("model"),
        "and the record it opened is the model call the cursor was on:\n{}",
        s.screen().contents()
    );
}

/// The reply the journey's model record carries: one numbered line per row, so
/// a test reads where the body is scrolled to off the screen.
fn journey_reply() -> String {
    (1..=40)
        .map(|n| format!("{BODY_LINE_PREFIX}{n:02}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The journey's session. Three turns, and the one in the middle holds a tool
/// record before the model record so the drill has to move the cursor to reach
/// the long body. The turns either side make "Esc returns to the turn it
/// opened" an assertion the pane's default tail position cannot satisfy by
/// accident.
fn journey_events(reply: &str) -> [SessionEvent; 8] {
    [
        SessionEvent::UserInput {
            text: "first prompt".into(),
        },
        SessionEvent::AssistantMessage {
            text: "first reply".into(),
            thinking: None,
        },
        SessionEvent::UserInput {
            text: "second prompt".into(),
        },
        SessionEvent::ToolCall {
            call_id: "call-short".into(),
            tool: "bash".into(),
            input: serde_json::json!({"command": "true"}),
        },
        SessionEvent::ToolResult {
            call_id: "call-short".into(),
            output: serde_json::json!({"stdout": "short tool body", "exit_code": 0}),
            duration_ms: 5,
        },
        SessionEvent::AssistantMessage {
            text: reply.to_string(),
            thinking: None,
        },
        SessionEvent::UserInput {
            text: "third prompt".into(),
        },
        SessionEvent::AssistantMessage {
            text: "third reply".into(),
            thinking: None,
        },
    ]
}

/// Seed the journey's session, resume it, and leave the /trajectory pane open
/// on the second turn: the turn holding the model record with the long reply.
fn open_journey_pane(sessions_dir: &Path, sid: &str) -> PtySession {
    let events = journey_events(&journey_reply());
    common::seed_session_events(sessions_dir, sid, "traj-model", &events);
    let mut s = PtySession::launch_with_sessions_dir(
        None,
        None,
        None,
        None,
        &["--resume".to_string(), sid.to_string()],
        sessions_dir.to_path_buf(),
    );
    assert!(
        s.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "working screen after sid resume:\n{}",
        s.output()
    );
    s.clear_output();
    run_slash_command(&mut s, "trajectory");
    // The pane opens on the newest turn, which is also how the test knows the
    // first page has landed.
    assert!(
        s.wait_for_screen("▸ T3", RENDER_TIMEOUT),
        "the pane opens on the newest turn:\n{}",
        s.screen().contents()
    );
    assert!(
        s.screen().contents().contains(TURN_LIST_FOOTER),
        "the turn list names the keys that act on it:\n{}",
        s.screen().contents()
    );
    s.send_key(&Key::Up);
    assert!(
        wait_until(&mut s, |screen| screen.contains("▸ T2")),
        "Up selects the second turn:\n{}",
        s.screen().contents()
    );
    s
}

/// A record body taller than the pane opens at its first line, pages ten lines
/// at a time, stops at its last, and the two Esc levels each come back to where
/// the drill started. Driven through the real terminal, so the pane's own key
/// routing and the frame each key acts on are what is under test.
#[test]
#[ignore]
fn test_trajectory_record_body_pages() {
    let sessions_dir = fresh_temp_dir("sessions-traj-record-pages");
    let sid = "1a1a1a1a-1a1a-1a1a-1a1a-1a1a1a1a1a1a";
    let mut s = open_journey_pane(&sessions_dir, sid);

    open_model_record(&mut s);
    assert_eq!(
        first_body_line(&s.screen().contents()),
        Some(1),
        "the body opens at its first line:\n{}",
        s.screen().contents()
    );
    let body = s.screen().contents();
    assert!(
        body.contains(RECORD_BODY_FOOTER),
        "the footer names the keys that act on the body:\n{body}"
    );

    // A page moves the body ten lines: the line at the foot of the first
    // window sits ten rows higher, still on screen.
    let foot = *body_rows(&s.screen().contents())
        .last()
        .expect("a body line on screen");
    s.send_key(&Key::PageDown);
    assert!(
        wait_until(&mut s, |screen| body_rows(screen)
            .iter()
            .any(|(row, number)| *number == foot.1 && *row + 10 == foot.0)),
        "PageDown moves the body ten lines:\n{}",
        s.screen().contents()
    );

    // Walking to the end brings the last line into view, and one more page
    // there changes nothing.
    for _ in 0..8 {
        s.send_key(&Key::PageDown);
    }
    assert!(
        wait_until(&mut s, |screen| screen
            .contains(&format!("{BODY_LINE_PREFIX}40"))),
        "the walk to the end brings the last line into view:\n{}",
        s.screen().contents()
    );
    let bottom = first_body_line(&s.screen().contents());
    s.send_key(&Key::PageDown);
    assert!(
        wait_until(&mut s, |screen| first_body_line(screen) == bottom
            && screen.contains(RECORD_BODY_FOOTER)),
        "one more page at the end changes nothing:\n{}",
        s.screen().contents()
    );

    // Esc returns to the record list on the record the drill opened, then to
    // the turn it opened.
    s.send_key(&Key::Esc);
    assert!(
        wait_until(&mut s, |screen| !screen.contains(RECORD_BODY_FOOTER)
            && selected_row(screen).is_some_and(|row| row.contains("model"))),
        "Esc returns to the record the drill opened:\n{}",
        s.screen().contents()
    );
    s.send_key(&Key::Esc);
    assert!(
        wait_until(&mut s, |screen| screen.contains("▸ T2")),
        "and again to the turn it opened:\n{}",
        s.screen().contents()
    );

    // The offset belongs to the record, so opening it again starts at the top.
    open_model_record(&mut s);
    assert!(
        wait_until(&mut s, |screen| first_body_line(screen) == Some(1)),
        "the body opens at its first line again:\n{}",
        s.screen().contents()
    );
    drop(s);
}

/// After a big resume, page-up to the top of the loaded rows and then stop
/// sending keys: the history read that loads the rows above must still land, so
/// the status total grows with no key left to wake the loop. The freeze this
/// guards against is the one where the read's delivery waits on an input tick —
/// a reader parked at the boundary never sees older rows.
#[test]
#[ignore]
fn test_history_loads_without_keys() {
    let sessions_dir = fresh_temp_dir("sessions-unfreeze");
    let sid = "44444444-4444-4444-4444-444444444444";
    // Sixteen mebibytes leaves the log far past the resident byte budget, so
    // the descent reaches the top of the loaded rows with older rows still
    // only on disk — the state where the next history read must be dispatched.
    // Replay time scales with the fixture, and the debug-profile binary is
    // several times slower than the release profile the measurement harness
    // times, so the settle timeout holds headroom for both.
    let fixture =
        seed_large_session_on_disk(&sessions_dir, sid, "unfreeze-model", 16 * 1024 * 1024);
    let mut s = PtySession::launch_with_sessions_dir(
        None,
        None,
        None,
        None,
        &["--resume".to_string(), sid.to_string()],
        sessions_dir.clone(),
    );
    // The replay ends on the tail that carries the settle marker, so reading
    // the screen is the settle signal: polling the accumulated output would
    // copy a multi-megabyte buffer on every pass of a replay this size.
    assert!(
        s.wait_for_screen(&fixture.settle_marker, Duration::from_secs(1500)),
        "replay of {} bytes did not settle:\n{}",
        fixture.log_bytes,
        s.output()
    );

    // Descend with page-ups until the status row reads line 1: the view has
    // reached the top of the loaded rows, and the pass that drew it dispatched
    // a read of the rows above. The total before that press is the baseline,
    // and a read landing after the press takes the total past it — which is
    // what the deadline below watches for. A press whose line has not reached
    // 1 by the window's end is a miss, and the descent presses again. The cap
    // sits far above the presses a descent of this size takes, so a slow render
    // costs a retry rather than the run: it bounds the loop for a view that
    // never reaches the top, which the check below then reports.
    let change_window = Duration::from_millis(300);
    let mut reached_top = false;
    let mut baseline = 0;
    let mut first_line: Option<u64> = None;
    for _ in 0..1400 {
        let before = parse_scroll_status(&s.screen().contents());
        let before_total = before.map(|(_, t)| t).unwrap_or(0);
        s.send_key(&Key::PageUp);
        let deadline = Instant::now() + change_window;
        while Instant::now() < deadline {
            sleep(Duration::from_millis(10));
            let now = parse_scroll_status(&s.screen().contents()).map(|(l, _)| l);
            if first_line.is_none() {
                first_line = now;
            }
            if now == Some(1) {
                reached_top = true;
                baseline = before_total;
                break;
            }
            if now != before.map(|(l, _)| l) {
                break;
            }
        }
        if reached_top {
            break;
        }
    }
    let parked = s.screen().contents();
    let (line, total) = parse_scroll_status(&parked).expect("the scroll status row reads");
    // The view starts at the tail, so the first press lands far below the top;
    // a descent that began at line 1 would reach the boundary without
    // exercising it, and this guard fails the run instead of passing quietly.
    assert!(
        first_line.is_some_and(|l| l > 1),
        "the descent must start below the loaded top (first line {first_line:?}):\n{parked}"
    );
    assert!(
        reached_top,
        "the descent never reached the top of the loaded rows (line {line}):\n{parked}"
    );

    // No more keys. A dispatched read must land on its own, taking the total
    // past the baseline within the deadline; a read whose delivery waits for an
    // input tick leaves the total frozen and this fails. The deadline covers
    // both the dispatch and the disk read behind it.
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut landed = false;
    while Instant::now() < deadline {
        sleep(Duration::from_millis(20));
        if let Some((_, total_now)) = parse_scroll_status(&s.screen().contents())
            && total_now > baseline
        {
            landed = true;
            break;
        }
    }
    let final_screen = s.screen().contents();
    assert!(
        landed,
        "no history read landed without keys: total {total}, baseline {baseline}, parked at line {line}. \
         A total that never left the baseline while the view sits at line {line} of {total} loaded rows \
         means the read the last press dispatched never arrived; a descent that stopped short of line 1 \
         would have failed the reached-top check above instead:\n{final_screen}"
    );
    drop(s);
}

/// A turn that switched models keeps that fact visible on a common terminal.
/// The list names the second model as a count and the drill states the switch,
/// so a reader does not have to widen the window to 148 columns to learn that
/// the turn ran on two models.
/// The events of a turn that ran on two models: one tool call, a measured
/// step on each model, and the cache break the switch caused.
fn switch_session_events() -> Vec<SessionEvent> {
    let usage = |turn: u32, model: &str| SessionEvent::TurnUsage {
        turn,
        call_in_turn: 1,
        input_tokens: 100,
        output_tokens: 10,
        cache_read_input_tokens: 90,
        cache_write_input_tokens: 0,
        reasoning_tokens: 0,
        model: model.to_string(),
        recovery: false,
        effort: None,
        input_reported: true,
        cache_read_reported: true,
        cache_write_reported: false,
    };
    let timing = |turn: u32, step: u32| SessionEvent::ModelStepTiming {
        turn,
        step,
        total_ms: 900,
        ttft_ms: Some(210),
        decode_ms: Some(600),
        reasoning_ms: None,
        response_ms: None,
    };
    let events = vec![
        SessionEvent::UserInput {
            text: "switched turn".into(),
        },
        SessionEvent::TurnStarted {
            turn: 1,
            call_in_turn: 0,
        },
        SessionEvent::AssistantMessage {
            text: "first reply".into(),
            thinking: None,
        },
        timing(1, 1),
        usage(1, "deepseek-v4-pro-0813"),
        SessionEvent::ToolCall {
            call_id: "call-1".into(),
            tool: "bash".into(),
            input: serde_json::json!({"command": "true"}),
        },
        SessionEvent::ToolResult {
            call_id: "call-1".into(),
            output: serde_json::json!({"stdout": "ok"}),
            duration_ms: 40,
        },
        SessionEvent::TurnStarted {
            turn: 2,
            call_in_turn: 0,
        },
        SessionEvent::AssistantMessage {
            text: "second reply".into(),
            thinking: None,
        },
        timing(2, 1),
        usage(2, "deepseek-v4.1-flash"),
        SessionEvent::CacheBreak {
            cause: "model-switch".into(),
        },
        SessionEvent::RunCompleted { ms: Some(1_000) },
    ];
    events
}

#[test]
#[ignore]
fn test_trajectory_switch_visible_narrow() {
    let sessions_dir = fresh_temp_dir("sessions-traj-switch");
    let sid = "56565656-5656-5656-5656-565656565656";
    let events = switch_session_events();
    common::seed_session_events(&sessions_dir, sid, "deepseek-v4-pro-0813", &events);
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
    s.resize(24, 130);
    s.clear_output();
    run_slash_command(&mut s, "trajectory");
    assert!(
        s.wait_for_plain("2 models", RENDER_TIMEOUT),
        "the header must name both models:\n{}",
        s.output_plain()
    );
    assert!(
        s.wait_for_plain("+1", RENDER_TIMEOUT),
        "the turn row must keep the switch at 130 columns:\n{}",
        s.output_plain()
    );
    // The tail of the row is what a column added on the left can push off the
    // edge, so the outcome and the counts are asserted on the same line as the
    // model cell.
    let screen = s.screen().contents();
    let row = screen
        .lines()
        .find(|line| line.contains("+1") && line.contains("✓"))
        .unwrap_or_else(|| panic!("no turn row with the model cell:\n{screen}"));
    assert!(
        row.contains("1 call"),
        "the row keeps its counts at 130 columns: {row:?}"
    );
    assert!(
        !screen.contains('┃'),
        "the list draws no vertical rules: {screen}"
    );

    s.clear_output();
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for_plain("-> deepseek-v4.1-flash", RENDER_TIMEOUT),
        "the drill must state the switch:\n{}",
        s.output_plain()
    );
    let detail = s.output_plain();
    assert!(
        detail.contains("models: deepseek-v4-pro-0813 → deepseek-v4.1-flash"),
        "the detail header states the chain:\n{detail}"
    );
    assert!(
        detail.contains("model 1") && detail.contains("TTFT"),
        "the timeline keeps the call ordinal and the wait:\n{detail}"
    );
    assert!(
        !detail.contains('┃'),
        "an instant draws no vertical rule:\n{detail}"
    );
    // A row's tail is what a narrow pane drops, so the model row must still end
    // with its outcome on the real screen, not only in the accumulated output.
    let detail_screen = s.screen().contents();
    let model_row = detail_screen
        .lines()
        .find(|line| line.contains("model 1") && line.contains("TTFT"))
        .unwrap_or_else(|| panic!("no model row on the level 1 screen:\n{detail_screen}"));
    assert!(
        model_row.trim_end().ends_with('✓'),
        "the model row keeps its outcome at 130 columns: {model_row:?}"
    );
    // The third level is where the raw identity is checked: the row above
    // shortens nothing, and the switch names the model it came from.
    s.clear_output();
    for _ in 0..3 {
        s.send_key(&Key::Down);
    }
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for_plain("switched", RENDER_TIMEOUT)
            && s.wait_for_plain("deepseek-v4-pro-0813", RENDER_TIMEOUT),
        "the record detail states what the call switched from:\n{}",
        s.output_plain()
    );
}
