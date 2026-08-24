//! Resume + status-provenance PTY journey tests, split out of ui_session on
//! the file-size gate. Drives the real binary through the sid-resume, lock
//! release (clean exit + crash), status provenance, and stale-cwd-degraded
//! journeys. Helpers are shared via the common module.

#![allow(clippy::unwrap_in_result)]

mod common;

use common::{Key, PtySession, RENDER_TIMEOUT, fresh_temp_dir, run_slash_command};
use houyicoder_core::{EventId, SessionId, TurnEvent, TurnEventKind};

/// --resume <sid> re-opens an existing session: the sid is REUSED (not a
/// fork), the model is restored from the sidecar, the seeded history stays,
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
    assert!(
        s.output().contains("sid-reopen-model"),
        "model should be restored from the sidecar:\n{}",
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
    let before = std::fs::read_to_string(&log_path).unwrap();
    assert!(
        before.contains("seeded prompt"),
        "seeded history should still be in the log:\n{before}"
    );
    let before_lines = before.lines().count();
    s.send_str("after reopen");
    s.send_key(&Key::Enter);
    std::thread::sleep(std::time::Duration::from_millis(1500));
    let after = std::fs::read_to_string(&log_path).unwrap();
    assert!(
        after.lines().count() > before_lines,
        "a continued turn should append to the same sid log:\n{after}"
    );
    assert!(
        after.contains("after reopen"),
        "the new turn should be in the reused sid log:\n{after}"
    );
}

/// The file lock is released on a clean exit: binary 1 --resume <sid> holds
/// the lock, quits cleanly (Esc at the login screen), then binary 2 --resume
/// <sid> acquires the released lock. The lock-rejects-second test covers the
/// contention case; this covers the release-on-exit invariant.
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
    s1.send_key(&Key::Char('q'));
    std::thread::sleep(std::time::Duration::from_millis(1000));
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
/// sidecar carries ResumedFromExport, projected to the wire + rendered).
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
    std::thread::sleep(std::time::Duration::from_millis(1000));
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
/// the binary runs in its launch cwd, the sidecar cwd is informational,
/// /status shows the stale recorded path. Guards the stale-sidecar-cwd
/// boundary.
#[test]
#[ignore]
fn test_sid_deleted_cwd_degrades() {
    let sessions_dir = fresh_temp_dir("sessions-cwd-deleted");
    let sid = "66666666-6666-6666-6666-666666666666";
    let event = TurnEvent {
        id: EventId::new(),
        session: SessionId::from_display_string(sid).unwrap(),
        ts: 0,
        prev_hash: None,
        kind: TurnEventKind::UserInput {
            text: "cwd-deleted session".into(),
        },
    };
    let dir = sessions_dir.join(sid);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
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
    std::fs::write(
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
    std::thread::sleep(std::time::Duration::from_millis(200));
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
    let sidecar = std::fs::read_to_string(sessions_dir.join(sid).join("session.json"))
        .unwrap_or_else(|_| String::new());
    assert!(
        sidecar.contains("\"name\": \"shiny-new-name\""),
        "sidecar should persist the renamed name:\n{sidecar}"
    );
    assert!(
        sidecar.contains("\"user\""),
        "sidecar should mark name_source=user:\n{sidecar}"
    );
}

/// User journey: a /resume while a run is in flight defers the swap. A run is in flight
/// (a 5s-delayed response), the user opens the /resume picker + Enter, the
/// swap is enqueued as a Command with a "will switch" hint + does NOT happen
/// while the run is live. After the run ends, the idle drain dispatches the
/// Command + swaps in-process, loading the target session's history.
#[test]
#[ignore]
fn test_during_run_defers_swap() {
    let sessions_dir = fresh_temp_dir("sessions-defer-resume");
    let sid_b = "99999999-9999-9999-9999-999999999999";
    common::seed_session_on_disk(&sessions_dir, sid_b, "defer-model-b", "session B prompt");
    // One-turn script: "done" text. The 5s delay keeps the run in flight
    // so the input is available while the run is live.
    let script = r#"[ [{"type":"Text","text":"done"}] ]"#;
    let mut s = PtySession::launch_with_sessions_dir(
        Some(script.to_string()),
        Some(5000),
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
    // Start a run (in flight during the 5s scripted delay).
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
        Some(5000),
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
    // Start a run (in flight during the 5s scripted delay).
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
    // The run ends -> idle drain: /resume Command sets pending_resume_target
    // -> next idle swap to the new session, carrying the queued message
    // across (demoted to ParkedMessage). The carried message then
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
        Some(5000),
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
        Some(5000),
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
    // archive notice (/clear drained) + run2's "done" instead.
    assert!(
        s.wait_for_plain("new session started", RENDER_TIMEOUT),
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
/// header reads "1 turns" (not "0 turns") + the turn row renders the seeded
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
    assert!(
        s.wait_for_plain("1 turns", RENDER_TIMEOUT),
        "trajectory header must report 1 seeded turn (not 0):\n{}",
        s.output_plain()
    );
    assert!(
        s.wait_for_plain("seeded trajectory prompt", RENDER_TIMEOUT),
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
    // Let the durable flush land before /export reads the log.
    std::thread::sleep(std::time::Duration::from_millis(1500));
    let export_path = sessions_dir.join("reexport.json");
    run_slash_command(&mut s, &format!("export {}", export_path.display()));
    assert!(
        s.wait_for("export: wrote", RENDER_TIMEOUT),
        "export should report a write after resume + continuation:\n{}",
        s.output()
    );
    let exported = std::fs::read_to_string(&export_path)
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
    let marker = std::env::temp_dir().join(format!(
        "houyi-resolve-marker-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    drop(std::fs::remove_file(&marker));
    // Built with serde_json so a temp path's backslashes on Windows escape
    // rather than break the JSON string.
    let settings = serde_json::json!({
        "apiKeyHelper": format!("echo >> {}", marker.display())
    });
    std::fs::create_dir_all(home.join(".houyicoder")).unwrap();
    std::fs::write(
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
    let after_start = std::fs::read_to_string(&marker).unwrap_or_default();
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
    let after_switch = std::fs::read_to_string(&marker).unwrap_or_default();
    assert_eq!(
        after_switch.lines().count(),
        1,
        "session switch must reuse the startup-resolved provider:\n{after_switch}"
    );
    drop(std::fs::remove_file(&marker));
    drop(std::fs::remove_dir_all(&home));
    drop(std::fs::remove_dir_all(&sessions_dir));
}
