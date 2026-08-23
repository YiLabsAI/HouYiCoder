//! Real-binary PTY tests for session-log persistence (the resume foundation).
//! The production binary persists every durable event to a file backend at the
//! sessions root; these tests drive one real turn through the binary and assert
//! the session log landed on disk with content. They are the end-to-end proof
//! of the disk-wiring (file backend + session store + append) that the unit
//! layer cannot reach, since the unit tier compiles the lib under cfg(test)
//! and gets the in-memory default instead.
//!
//! Run via make test ui (builds the bin first) or
//! cargo test --test ui_session -- --ignored after cargo build --bin houyi.

#![allow(clippy::unwrap_in_result)]

mod common;

use common::{Key, PtySession, RENDER_TIMEOUT, fresh_temp_dir, session_on_working_with_script};
use houyicoder_core::{EventId, SessionId, TurnEvent, TurnEventKind};

/// A single-response script: plain text only, so the run completes in one
/// step (no tool call, no approval pause). The durable events that land on
/// disk are the user input + the assistant text reply.
const ONE_REPLY_SCRIPT: &str = r#"[{"type":"Text","text":"logged"}]"#;

/// Drive one turn through the real binary and assert the session log file
/// exists on disk with non-empty content. Proves the production wiring
/// flushes durable events to disk per turn -- the resume precondition, and
/// the mirror guard of the default-in-memory isolation: a production entry
/// that loses its disk opt-in turns this red. The sessions root is isolated
/// to a per-launch temp dir by the harness, so the assertion lands on the
/// test's own dir, never the developer home.
#[test]
#[ignore]
fn test_turn_writes_durable_log() {
    let mut s = session_on_working_with_script(ONE_REPLY_SCRIPT);
    s.send_str("say logged");
    s.send_key(&Key::Enter);
    s.wait_for("logged", RENDER_TIMEOUT);

    // The sessions root holds one sid dir; that dir holds log.jsonl (the
    // durable events from the turn just driven) plus the session.json
    // sidecar the lazy-materialize hook writes on the first durable append.
    // Poll for the whole shape rather than checking once: the reply renders
    // without waiting for the durable append to complete, so at the moment
    // the reply text is on screen the sid dir may not exist yet and the
    // sidecar's write-tmp-then-rename publish may still be in flight. A
    // one-shot check can land inside that window and see a stray tmp with no
    // session.json. Polling is the correct wait, not a sleep.
    let root = s.sessions_dir();
    let sid_dir = wait_for_session_files(root, &["log.jsonl", "session.json"], RENDER_TIMEOUT);
    let sid_dir = sid_dir.unwrap_or_else(|| {
        panic!(
            "no session dir under {root:?} holding log.jsonl + session.json:\n{}",
            s.output()
        )
    });
    let log = sid_dir.join("log.jsonl");
    let body = std::fs::read_to_string(&log).unwrap();
    assert!(!body.is_empty(), "session log empty: {log:?}");
    // The log is one JSON object per durable line; the assistant reply text
    // is the canonical signal the turn landed. The exact field shape is
    // owned by the context layer, so assert on the rendered text only.
    assert!(
        body.contains("logged") || body.contains("say logged"),
        "session log should carry the turn text:\n{body}"
    );

    // The session.json sidecar is written on the first durable append (the
    // lazy-materialize hook), so it holds the model + the workspace cwd the
    // session started in. Proves the metadata sidecar wiring (composition ->
    // FileMetaStore -> disk) lands on the prod path the resume + /status
    // paths will read.
    let meta_path = sid_dir.join("session.json");
    let meta = std::fs::read_to_string(&meta_path).unwrap();
    assert!(!meta.is_empty(), "session.json empty: {meta_path:?}");
    assert!(
        meta.contains("model"),
        "session.json should carry the model field:\n{meta}"
    );
    assert!(
        meta.contains("cwd"),
        "session.json should carry the cwd field:\n{meta}"
    );
}

/// Poll the sessions root until one session dir holds every named file,
/// and return that dir. The directory lookup is inside the loop, not just
/// the leaf files: the sid dir is created by the durable append, which the
/// reply render does not wait for, so the dir is exactly as racy as the
/// files under it. Each name is checked with is_file, so a directory of
/// that name does not satisfy it. None on timeout, so the caller attaches
/// the PTY output to the failure the way the wait_for helpers do.
fn wait_for_session_files(
    root: &std::path::Path,
    names: &[&str],
    timeout: std::time::Duration,
) -> Option<std::path::PathBuf> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let found = std::fs::read_dir(root)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|e| e.path())
            .find(|p| p.is_dir() && names.iter().all(|n| p.join(n).is_file()));
        if found.is_some() {
            return found;
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// A fixture export file: two durable events (a user prompt + an assistant
/// reply) with a legacy ULID session id, so the resume path also exercises
/// the tolerant SessionId deserialize (a pre-change export resumes). The
/// prev_hash chain is intentionally absent (all None) -- the seed's source
/// verify marks it Unverified but the durable rebuild proceeds (the new
/// session's chain is internally self-consistent), proving the tolerant
/// degrade path the design calls for.
fn write_resume_fixture() -> std::path::PathBuf {
    let legacy_sid = "01KZ5RDH4DG6YV0EDBX1KSKTRA"; // legacy ULID (pre-change)
    let sid = SessionId::from_display_string(legacy_sid).expect("legacy ULID parses");
    let mk = |kind: TurnEventKind| TurnEvent {
        id: EventId::new(),
        session: sid,
        ts: 0,
        prev_hash: None,
        kind,
    };
    let events = vec![
        mk(TurnEventKind::UserInput {
            text: "resumed hello from export".into(),
        }),
        mk(TurnEventKind::AssistantMessage {
            text: "resumed reply from export".into(),
            thinking: None,
        }),
    ];
    // Minimal export slice the resume path deserializes (session_id + model
    // + trajectory; serde ignores the derived stats fields a full export
    // carries, so adding fields later does not break resume).
    let doc = serde_json::json!({
        "session_id": legacy_sid,
        "model": "stub-resume-model",
        "trajectory": events,
    });
    let dir = std::env::temp_dir().join(format!(
        "houyi-resume-fixture-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("mkdir fixture dir");
    let path = dir.join("export.json");
    std::fs::write(&path, serde_json::to_string_pretty(&doc).unwrap()).expect("write fixture");
    path
}

/// --resume <file>: spawn the binary with an export file, land on the
/// working surface, and assert the seeded history landed in the new
/// session's log on disk + a fresh turn continues into the same log.
/// Proves the file branch end-to-end: read -> deserialize (tolerant of
/// the legacy ULID) -> seed -> assemble -> the runner's session log
/// carries the seeded durable events + a continued turn appends to it.
/// The TUI showing the resumed history on the working surface is the
/// slash /resume de-stub (a separate acceptance row); the launch path
/// only needs the model to see the replayed history + the log to persist.
#[test]
#[ignore]
fn test_resume_export_persists_history() {
    let fixture = write_resume_fixture();
    let mut s = PtySession::launch_with_args(
        None,
        None,
        None,
        None,
        &[
            "--resume".to_string(),
            fixture.to_string_lossy().into_owned(),
        ],
    );
    // Pick local mode (3) to clear the login screen + land on the working
    // surface. The status line shows the export's model name, proving the
    // model was restored (not the default resolve_model()).
    // With skip_login, --resume goes straight to the working screen.
    assert!(
        s.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "working screen should render (skip_login):\n{}",
        s.output()
    );
    assert!(
        s.output().contains("stub-resume-model"),
        "the export model should be restored on the status line:\n{}",
        s.output()
    );

    // The new session's log on disk carries the seeded durable events (the
    // user prompt + assistant reply from the fixture), proving seed ->
    // disk. The new sid dir is the only one under the isolated sessions root.
    let root = s.sessions_dir();
    let log = std::fs::read_dir(root)
        .unwrap_or_else(|e| panic!("read sessions root {root:?}: {e}"))
        .filter_map(Result::ok)
        .map(|e| e.path().join("log.jsonl"))
        .find(|p| p.exists())
        .unwrap_or_else(|| panic!("no session log under {root:?}:\n{}", s.output()));
    let body = std::fs::read_to_string(&log).unwrap();
    assert!(
        body.contains("resumed hello from export"),
        "seeded user prompt should be in the log:\n{body}"
    );
    assert!(
        body.contains("resumed reply from export"),
        "seeded assistant reply should be in the log:\n{body}"
    );

    // A continued turn appends to the same log (the resume precondition:
    // the new session continues the conversation on disk). Drive a stub
    // reply + assert the log grew beyond the seeded events.
    let seeded_len = body.lines().count();
    s.send_str("continue");
    s.send_key(&Key::Enter);
    // The stub provider replies (no API key); wait for the turn to land.
    std::thread::sleep(std::time::Duration::from_millis(1500));
    let body2 = std::fs::read_to_string(&log).unwrap();
    assert!(
        body2.lines().count() > seeded_len,
        "a continued turn should append to the resumed log:\n{body2}"
    );

    drop(std::fs::remove_dir_all(
        fixture.parent().expect("fixture has a parent dir"),
    ));
}

/// --resume <sid> where the sid does not exist on disk: the binary surfaces
/// a "no session log" error and exits rather than crashing or silently
/// starting a fresh session. Proves the sid branch's existence check + the
/// error-to-stderr path. (The success path --resume <existing-sid> reuses
/// the same assemble + wire_bundle path the file branch verifies, so it is
/// covered by resume_from_export_persists_history; the novel sid-branch code
/// is the existence check + sidecar model restore, exercised here.)
#[test]
#[ignore]
fn test_resume_missing_reports_error() {
    // A valid-shape UUID that is not on disk: parses as a sid, then the log
    // existence check fails.
    let missing = "00000000-0000-0000-0000-000000000000";
    let mut s = PtySession::launch_with_args(
        None,
        None,
        None,
        None,
        &["--resume".to_string(), missing.to_string()],
    );
    // The binary prints the error + exits (no login screen). Poll for the
    // error line (the reader thread drains the PTY into the output buffer).
    assert!(
        s.wait_for("no session log", RENDER_TIMEOUT),
        "a missing sid should surface a no-session-log error, got:\n{}",
        s.output()
    );
}

/// Write a session on disk (log.jsonl + session.json) so a subsequent
/// --resume <sid> or the /resume picker can re-open it. The log carries one
/// UserInput event (a genesis line with prev_hash null, so the chain
/// verifies); the sidecar carries the model the resume path should restore.
fn seed_session_on_disk(root: &std::path::Path, sid_str: &str, model: &str, prompt: &str) {
    let sid = SessionId::from_display_string(sid_str).expect("sid parses");
    let event = TurnEvent {
        id: EventId::new(),
        session: sid,
        ts: 0,
        prev_hash: None,
        kind: TurnEventKind::UserInput {
            text: prompt.into(),
        },
    };
    let line = serde_json::to_string(&event).expect("serialize event");
    let dir = root.join(sid_str);
    std::fs::create_dir_all(&dir).expect("mkdir session dir");
    std::fs::write(dir.join("log.jsonl"), format!("{line}\n")).expect("write log");
    let meta = serde_json::json!({
        "name": null,
        "name_source": "auto",
        "cwd": "/tmp",
        "model": model,
        "provenance": {"kind": "fresh"},
        "version": "test",
        "created_at": 1000,
    });
    std::fs::write(
        dir.join("session.json"),
        serde_json::to_string_pretty(&meta).expect("serialize meta"),
    )
    .expect("write sidecar");
}

/// /resume picker in-process swap e2e: seed a session A on disk, launch the
/// binary (which starts in a fresh session B), open the picker, select A, and
/// verify the event loop swaps to A in place -- no quit, no re-login, the
/// working screen re-renders with A's model and a continued turn appends to
/// A's log on disk (not B's), proving the session switched. The swap
/// sets the session id, rebuilds the bundle, keeps
/// running the same event loop. This is the only path that exercises the
/// full swap the unit tier cannot reach (the loop is in the binary, not the
/// lib).
#[test]
#[ignore]
fn test_resume_picker_swaps_process() {
    let sessions_dir = common::fresh_temp_dir("sessions-swap");
    let sid_a = "11111111-1111-1111-1111-111111111111";
    seed_session_on_disk(&sessions_dir, sid_a, "stub-model-a", "seeded prompt a");

    let mut s =
        PtySession::launch_with_sessions_dir(None, None, None, None, &[], sessions_dir.clone());
    // Land on the working screen (session B is fresh).
    assert!(s.wait_for("sign in to houyicoder", RENDER_TIMEOUT));
    s.send_key(&Key::Char('3'));
    assert!(
        s.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "working screen after local login:\n{}",
        s.output()
    );

    // Open the /resume picker: it lists A (B is excluded as current).
    common::run_slash_command(&mut s, "resume");
    assert!(
        s.wait_for_plain("Resume a session", RENDER_TIMEOUT),
        "picker should open with the session list:\n{}",
        s.output()
    );
    assert!(
        s.output_plain().contains("seeded-prompt-a"),
        "picker should show session A's title:\n{}",
        s.output()
    );
    // Select A (the only row, already selected) + Enter to switch.
    s.send_key(&Key::Enter);
    // In-process swap: the event loop calls resume_builder, swaps the
    // session in-place, and continues (no restart, no login screen).
    s.clear_output();
    assert!(
        s.wait_for_plain("stub-model-a", RENDER_TIMEOUT),
        "working screen after in-process swap:\n{}",
        s.output_plain()
    );
    // Drive a turn: the appended events must land in A's log, not B's.
    let log_a = sessions_dir.join(sid_a).join("log.jsonl");
    let before = std::fs::read_to_string(&log_a).unwrap_or_default();
    let before_lines = before.lines().count();
    s.send_str("after switch");
    s.send_key(&Key::Enter);
    // The stub provider replies; wait for the turn text to land on disk.
    std::thread::sleep(std::time::Duration::from_millis(1500));
    let after = std::fs::read_to_string(&log_a).unwrap_or_default();
    assert!(
        after.lines().count() > before_lines,
        "in-process swap should append to session A's log:\nbefore({before_lines}):\n{before}\nafter:\n{after}"
    );
    assert!(
        after.contains("after switch"),
        "the continued turn should be in A's log:\n{after}"
    );
}

/// Two processes resume the same sid: the second --resume <sid> is rejected
/// at the file lock (the hash chain's single-writer invariant). The second
/// binary surfaces a "held by another live process" error + exits, not a
/// silent double-write. Proves the run_tui_loop lock acquisition path the
/// unit tier cannot reach (the loop is in the binary). The SessionLock unit
/// test covers the lock semantics; this test covers the wiring.
#[test]
#[ignore]
fn test_resume_lock_rejects_second() {
    let sessions_dir = common::fresh_temp_dir("sessions-lock");
    let sid_a = "22222222-2222-2222-2222-222222222222";
    seed_session_on_disk(&sessions_dir, sid_a, "stub-model-lock", "locked session");

    // Binary 1: --resume <sid_a> acquires the lock + holds it on the login
    // screen (the TUI path does not write a pidfile, so is_session_live
    // returns false; the flock is what blocks the second resume). Wait for
    // the login screen so the lock is provably held before binary 2 starts.
    let mut s1 = PtySession::launch_with_sessions_dir(
        None,
        None,
        None,
        None,
        &["--resume".to_string(), sid_a.to_string()],
        sessions_dir.clone(),
    );
    assert!(
        s1.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "binary 1 should reach the working screen (lock held):\n{}",
        s1.output()
    );

    // Binary 2: --resume <sid_a> hits the lock + is rejected.
    let mut s2 = PtySession::launch_with_sessions_dir(
        None,
        None,
        None,
        None,
        &["--resume".to_string(), sid_a.to_string()],
        sessions_dir.clone(),
    );
    assert!(
        s2.wait_for_plain("held by another live process", RENDER_TIMEOUT)
            || s2.wait_for_plain("fork a new session", RENDER_TIMEOUT),
        "second resume should be rejected at the lock:\n{}",
        s2.output()
    );
}

/// List the session-id dirs (each a sid directory) under a sessions root.
/// Files (the export json, lock files) are filtered out.
fn sid_dirs(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(root)
        .unwrap_or_else(|e| panic!("read sessions root {root:?}: {e}"))
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect()
}

/// User journey: export a LIVE session mid-run, then resume that export
/// while the source is still alive. The resume mints a NEW sid (a fork,
/// not an adopt), so the two processes write independent logs with no lock
/// contention and no double-write. Proves the live-export-then-resume path
/// the design claims is safe-by-construction: the fork carries the source
/// history at export time, the live source keeps running and its log is
/// untouched by the fork's subsequent writes. The lock test covers same-sid
/// contention; the export-resume test covers a not-live source; only this
/// one covers the live-source fork the user actually hits.
#[test]
#[ignore]
fn test_resume_export_live_session() {
    let sessions_dir = common::fresh_temp_dir("sessions-live-export");
    // Session A: alive, drives one turn, then exports mid-run.
    let mut a = PtySession::launch_with_sessions_dir(
        Some(ONE_REPLY_SCRIPT.to_string()),
        None,
        None,
        None,
        &[],
        sessions_dir.clone(),
    );
    assert!(
        a.wait_for("sign in to houyicoder", RENDER_TIMEOUT),
        "A should reach the login screen"
    );
    a.send_key(&Key::Char('3'));
    assert!(
        a.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "A working screen"
    );
    a.send_str("hello from A");
    a.send_key(&Key::Enter);
    // Wait for the user input to echo in A's transcript (the turn landed +
    // the durable UserInput event flushed). The stub provider here is the
    // no-api-key fallback (it prints a "stub mode" line, not a scripted
    // reply); the fork semantics under test only need the user input to be
    // durable so the export carries it -- the assistant reply is irrelevant.
    assert!(
        a.wait_for("hello from A", RENDER_TIMEOUT),
        "A user input should echo:\n{}",
        a.output()
    );
    // Let the durable flush land on disk before the export reads the log.
    std::thread::sleep(std::time::Duration::from_millis(500));

    // Snapshot A's sid before B spawns (A is the only sid so far).
    let sid_a = sid_dirs(&sessions_dir)
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("A sid dir missing under {sessions_dir:?}"));

    // Export to a known path while A is alive.
    let export_path = sessions_dir.join("live-export.json");
    common::run_slash_command(&mut a, &format!("export {}", export_path.display()));
    assert!(
        a.wait_for("export: wrote", RENDER_TIMEOUT),
        "export should report a write while A is alive:\n{}",
        a.output()
    );

    // Session B: resume the export while A is still alive. Same sessions
    // root so both sid dirs land side-by-side (the lock is per-sid, so no
    // contention).
    let mut b = PtySession::launch_with_sessions_dir(
        Some(ONE_REPLY_SCRIPT.to_string()),
        None,
        None,
        None,
        &[
            "--resume".to_string(),
            export_path.to_string_lossy().into_owned(),
        ],
        sessions_dir.clone(),
    );
    assert!(
        b.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "B working screen after resume (skip_login):\n{}",
        b.output()
    );

    // B mints a NEW sid (fork, not adopt) -> differs from A.
    let sids = sid_dirs(&sessions_dir);
    assert!(sids.len() >= 2, "A + B should both have sid dirs: {sids:?}");
    let sid_b = sids
        .iter()
        .find(|p| *p != &sid_a)
        .unwrap_or_else(|| panic!("B sid (new, != A) missing: {sids:?}"));

    // B's log carries A's exported history (the turn A drove before export).
    let log_b = std::fs::read_to_string(sid_b.join("log.jsonl"))
        .unwrap_or_else(|e| panic!("read B log: {e}"));
    assert!(
        log_b.contains("hello from A"),
        "B should be seeded with A's exported history:\n{log_b}"
    );

    // A's log line count before B drives a turn.
    let log_a_path = sid_a.join("log.jsonl");
    let a_before = std::fs::read_to_string(&log_a_path)
        .unwrap_or_else(|e| panic!("read A log: {e}"))
        .lines()
        .count();

    // B drives its own turn -> appends to B's log, NOT A's.
    b.send_str("from B");
    b.send_key(&Key::Enter);
    std::thread::sleep(std::time::Duration::from_millis(1500));
    let log_b_after = std::fs::read_to_string(sid_b.join("log.jsonl")).unwrap();
    assert!(
        log_b_after.contains("from B"),
        "B's turn should land in B's log:\n{log_b_after}"
    );

    // A's log untouched by B's fork writes (no double-write to the source).
    let a_after = std::fs::read_to_string(&log_a_path)
        .unwrap()
        .lines()
        .count();
    assert_eq!(
        a_after, a_before,
        "A's log must be unchanged by B's fork writes: before={a_before} after={a_after}"
    );

    // A is still alive (its PTY still drains). The export + fork did not
    // kill the source.
    assert!(
        a.output().contains("hello from A"),
        "A should still be alive + its earlier input still in its output"
    );
}

/// User journey: /status in a real binary renders the session identity
/// block (Version / Session name / Session ID / cwd / Model / provenance)
/// from the sidecar the server attaches to the wire snapshot. The unit
/// tier asserts the render function with a synthetic snapshot; this drives
/// the full wire path end-to-end (server reads the sidecar -> projects to
/// the wire summary -> TUI renders) so a wiring break between the server
/// Before the first turn, no durable append has fired and no sidecar is on
/// disk. Version is the running build (top-level on the snapshot, set by the
/// server) so it renders; cwd and provenance come from the sidecar and drop
/// honestly. This assertion encodes the post-deferral contract: a fresh
/// session shows Version, not a fabricated cwd or provenance.
#[test]
#[ignore]
fn test_status_version_before_turn() {
    let mut s = common::session_on_working_with_script_rows(ONE_REPLY_SCRIPT, 40);
    common::run_slash_command(&mut s, "status");
    assert!(
        s.wait_for_plain("Version:", RENDER_TIMEOUT),
        "/status should render Version before the first turn:\n{}",
        s.output()
    );
    let out = s.output_plain();
    assert!(
        !out.contains("cwd:"),
        "cwd must drop before the sidecar lands:\n{out}"
    );
    assert!(
        !out.contains("provenance:"),
        "provenance must drop before the sidecar lands:\n{out}"
    );
}

/// --continue resumes the most-recently-active session with no sid needed.
/// Seed A on disk, launch with --continue, land on the working screen with
/// A's restored model + A's history visible in the transcript. The
/// serve-start replay + restore_trajectory backfill ship the history.
#[test]
#[ignore]
fn test_continue_resumes_latest_session() {
    let sessions_dir = common::fresh_temp_dir("sessions-continue");
    let sid_a = "44444444-4444-4444-4444-444444444444";
    common::seed_session_on_disk(&sessions_dir, sid_a, "continue-model", "seeded prompt a");
    let mut s = PtySession::launch_with_sessions_dir(
        None,
        None,
        None,
        None,
        &["--continue".to_string()],
        sessions_dir,
    );
    assert!(
        s.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "working screen after --continue:\n{}",
        s.output()
    );
    assert!(
        s.output().contains("continue-model"),
        "--continue should restore the latest session's model:\n{}",
        s.output()
    );
    assert!(
        s.wait_for("seeded prompt a", RENDER_TIMEOUT),
        "--continue should show the resumed history:\n{}",
        s.output()
    );
}

/// --continue with no session in the current workspace errors with guidance,
/// rather than silently jumping to a session in another workspace. The strict
/// cwd convergence means no cross-repo fallback; the user is told how to
/// proceed (--resume <sid> / start fresh).
#[test]
#[ignore]
fn test_continue_no_session_errors() {
    let sessions_dir = common::fresh_temp_dir("sessions-continue-none");
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
        "--continue with no session in the workspace should error with guidance, not jump:\n{}",
        s.output_plain()
    );
}

/// --resume <sid> --fork-session mints a new sid seeded from the source; the
/// source's log is untouched by the fork's continued turn. Seed A, fork it,
/// drive a turn, assert the new turn lands in a DIFFERENT log file (the new
/// sid), not A's.
#[test]
#[ignore]
fn test_fork_keeps_source_untouched() {
    let sessions_dir = common::fresh_temp_dir("sessions-fork");
    let sid_a = "66666666-6666-6666-6666-666666666666";
    common::seed_session_on_disk(&sessions_dir, sid_a, "fork-model", "seeded prompt a");
    let mut s = PtySession::launch_with_sessions_dir(
        None,
        None,
        None,
        None,
        &[
            "--resume".to_string(),
            sid_a.to_string(),
            "--fork-session".to_string(),
        ],
        sessions_dir.clone(),
    );
    assert!(
        s.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "working screen after fork:\n{}",
        s.output()
    );
    assert!(
        s.output().contains("fork-model"),
        "fork should restore the source's model:\n{}",
        s.output()
    );
    assert!(
        s.wait_for("seeded prompt a", RENDER_TIMEOUT),
        "fork should seed the source's history:\n{}",
        s.output()
    );
    // A continued turn must NOT append to A's log (the source is untouched);
    // it lands in the new sid's log. Count A's lines before + after.
    let log_a = sessions_dir.join(sid_a).join("log.jsonl");
    let before = std::fs::read_to_string(&log_a).unwrap_or_default();
    let before_lines = before.lines().count();
    s.send_str("after fork");
    s.send_key(&Key::Enter);
    std::thread::sleep(std::time::Duration::from_millis(1500));
    let after = std::fs::read_to_string(&log_a).unwrap_or_default();
    assert_eq!(
        after.lines().count(),
        before_lines,
        "fork's continued turn must NOT touch the source log:\nbefore({before_lines}):\n{before}\nafter:\n{after}"
    );
}

/// A sessions store over the retention count cap surfaces as a startup
/// system line pointing at the review path. The background sweep skips
/// auto-apply above the threshold, so without this line a backlog is
/// invisible until the user thinks to ask. Seeds a low cap (2) in the
/// isolated HOME settings + three sessions in the isolated sessions dir,
/// then asserts the notice reaches the transcript on launch - the same
/// end-to-end wiring the stale-catalog startup warning proves by.
#[test]
#[ignore]
fn test_backlog_notice_warns_startup() {
    let home = fresh_temp_dir("backlog-warn-home");
    std::fs::create_dir_all(home.join(".houyicoder")).unwrap();
    std::fs::write(
        home.join(".houyicoder").join("settings.json"),
        r#"{"session_retention_count": 2}"#,
    )
    .unwrap();
    let sessions_dir = fresh_temp_dir("backlog-warn-sessions");
    for i in 0..3 {
        seed_session_on_disk(
            &sessions_dir,
            &format!("00000000-0000-0000-0000-00000000000{i}"),
            "test",
            "",
        );
    }
    let mut s =
        PtySession::launch_with_sessions_dir(None, None, Some(home), None, &[], sessions_dir);
    assert!(s.wait_for("sign in to houyicoder", RENDER_TIMEOUT), "login");
    s.send_key(&Key::Char('3'));
    assert!(
        s.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "working screen"
    );
    assert!(
        s.wait_for_plain(
            "session store holds 3 sessions, over the retention count of 2",
            RENDER_TIMEOUT,
        ),
        "backlog notice surfaces at startup: {}",
        s.output_plain()
    );
}
