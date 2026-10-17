//! Peer tests for the resume entry points: from an export file, from a session
//! id, and forking into a new id. A resumed session must carry the source
//! history forward without mutating the source, restore the model that session
//! used rather than the configured default, and refuse an untrusted source
//! instead of silently starting a blank session the user thinks is their old one.

use super::super::workspace_cwd;
use houyicoder_context::{
    EventId, NameSource, SessionDescriptor, SessionEvent, SessionId, SessionLogEntry,
    SessionProvenance,
};
use houyicoder_memory::FileDescriptorStore;
use houyicoder_provider::ProviderResolution;
use std::env::temp_dir;
use std::fs::create_dir_all;
use std::fs::read_dir;
use std::fs::read_to_string;
use std::fs::remove_dir_all;
use std::fs::remove_file;
use std::fs::write;
use std::path::Path;
use std::path::PathBuf;
use std::process::id;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use tokio::runtime::Builder;

/// Serialize a minimal export slice (session_id + model + trajectory) so the
/// resume path can deserialize it. serde ignores the derived-stats fields a
/// full export carries, so this slice round-trips through resume.
fn write_export(path: &Path, session_id: &str, model: &str, events: &[SessionLogEntry]) {
    let doc = serde_json::json!({
        "session_id": session_id,
        "model": model,
        "trajectory": events,
    });
    write(path, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
}

fn two_events(sid: SessionId) -> Vec<SessionLogEntry> {
    let mk = |kind: SessionEvent| SessionLogEntry {
        id: EventId::new(),
        session: sid,
        ts: 0,
        prev_hash: None,
        event: kind,
    };
    vec![
        mk(SessionEvent::UserInput {
            text: "resume-unit-prompt".into(),
        }),
        mk(SessionEvent::AssistantMessage {
            text: "resume-unit-reply".into(),
            thinking: None,
        }),
    ]
}

/// One recorded turn as the session reader hands it back: the tokens the
/// endpoint reported plus the flags saying each figure was reported at all.
fn usage_events(sid: SessionId) -> Vec<SessionLogEntry> {
    let mk = |session_event: SessionEvent| SessionLogEntry {
        id: EventId::new(),
        session: sid,
        ts: 0,
        prev_hash: None,
        event: session_event,
    };
    vec![
        mk(SessionEvent::UserInput {
            text: "resume-usage-prompt".into(),
        }),
        mk(SessionEvent::TurnUsage {
            turn: 1,
            call_in_turn: 1,
            input_tokens: 12_000,
            output_tokens: 500,
            cache_read_input_tokens: 9_600,
            cache_write_input_tokens: 0,
            reasoning_tokens: 0,
            model: "unit-resume-model".into(),
            recovery: false,
            effort: None,
            input_reported: true,
            cache_read_reported: true,
            cache_write_reported: false,
        }),
    ]
}

/// A resumed session reports the spend its history already holds. The turns
/// the export carried are folded into the runner's figures at assembly, so the
/// cumulative covers the whole session the delegated half covers rather than
/// only the turns run after the reopen, and the per-model rows the same panel
/// shows cover that same session.
#[test]
fn test_resume_export_keeps_usage() {
    let sessions = temp_root();
    let export_path = sessions.join("export.json");
    let legacy_sid = "01KZ5RDH4DG6YV0EDBX1KSKTRA";
    let sid = SessionId::from_display_string(legacy_sid).unwrap();
    write_export(&export_path, legacy_sid, "usage-model", &usage_events(sid));

    let resumed = super::build_runner_for_resume_export(
        &export_path,
        &sessions,
        None,
        None,
        ProviderResolution::stub(),
    )
    .expect("resume export seeds");
    let snap = resumed.assembled.runner.status_snapshot();
    assert_eq!(
        snap.cumulative_usage.input_tokens, 12_000,
        "the resumed session's input covers the turn its log held"
    );
    assert_eq!(snap.cumulative_usage.output_tokens, 500);
    assert_eq!(snap.cumulative_usage.cache_read_input_tokens, 9_600);
    assert_eq!(snap.last_input_tokens, 12_000);
    assert_eq!(
        snap.cumulative_usage.cache_hit_rate(),
        Some(0.8),
        "the recorded turn's completeness survives the fold"
    );
    let rows = resumed.assembled.runner.by_model_usage();
    assert_eq!(rows.len(), 1, "the logged model keeps its row");
    assert_eq!(rows[0].0, "unit-resume-model");
    assert_eq!(
        rows[0].1.input_tokens, 12_000,
        "the rows cover the turns the cumulative covers"
    );
    assert_eq!(rows[0].1.cache_read_tokens, 9_600);
    remove_dir_all(&sessions).ok();
}

fn temp_root() -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let p = temp_dir().join(format!("composition-resume-{}-{n}", id()));
    create_dir_all(&p).unwrap();
    p
}

/// build_runner_for_resume_export seeds the new session's log on disk +
/// restores the export's model. The returned runner's store replays the
/// seeded durable events (the engine sees the history on the next run).
#[test]
fn test_resume_export_seeds_log() {
    let sessions = temp_root();
    let export_path = sessions.join("export.json");
    let legacy_sid = "01KZ5RDH4DG6YV0EDBX1KSKTRA";
    let sid = SessionId::from_display_string(legacy_sid).unwrap();
    write_export(
        &export_path,
        legacy_sid,
        "unit-resume-model",
        &two_events(sid),
    );

    let resumed = super::build_runner_for_resume_export(
        &export_path,
        &sessions,
        None,
        None,
        ProviderResolution::stub(),
    )
    .expect("resume export seeds");
    let runner = resumed.assembled.runner;
    let new_sid = resumed.assembled.session;
    let model = resumed.model;

    // New session is minted (fork), not the legacy source sid.
    assert_ne!(new_sid.to_string(), legacy_sid);
    assert_eq!(model, "unit-resume-model");
    // The seeded events landed in the new session's log on disk + the
    // runner's store replays them.
    let rt = Builder::new_current_thread().enable_all().build().unwrap();
    let events = rt.block_on(async { runner.store().replay(new_sid).await.unwrap() });
    assert!(events.len() >= 2, "seeded events should replay: {events:?}");
    // The log state was filled before assembly, so the extractor cursor is
    // seeded to the restored tail: the history is never re-extracted.
    assert_eq!(
        runner.extractor_cursor(),
        events.last().map(|e| e.id),
        "the restored tail seeds the extractor cursor"
    );
    let body = read_to_string(sessions.join(new_sid.to_string()).join("log.jsonl")).unwrap();
    assert!(
        body.contains("resume-unit-prompt"),
        "seeded prompt in log: {body}"
    );
    remove_dir_all(&sessions).ok();
}

/// After an export-resume, the new session's descriptor carries
/// provenance=ResumedFromExport pointing back at the source session id
/// The resumed descriptor preserves its export provenance for status views.
#[test]
fn test_resume_export_descriptor_provenance() {
    let sessions = temp_root();
    let export_path = sessions.join("export.json");
    let legacy_sid = "01KZ5RDH4DG6YV0EDBX1KSKTRA";
    let sid = SessionId::from_display_string(legacy_sid).unwrap();
    write_export(&export_path, legacy_sid, "prov-model", &two_events(sid));
    let resumed = super::build_runner_for_resume_export(
        &export_path,
        &sessions,
        None,
        None,
        ProviderResolution::stub(),
    )
    .expect("resume export seeds");
    let new_sid = resumed.assembled.session;

    let descriptor = read_to_string(sessions.join(new_sid.to_string()).join("session.json"))
        .expect("descriptor exists after resume");
    let descriptor: serde_json::Value =
        serde_json::from_str(&descriptor).expect("descriptor is json");
    let prov = descriptor
        .get("provenance")
        .expect("descriptor has provenance")
        .as_object()
        .expect("provenance is an object");
    assert_eq!(
        prov.get("kind").and_then(|v| v.as_str()),
        Some("resumed_from_export"),
        "provenance kind should be resumed_from_export:\n{descriptor}"
    );
    assert_eq!(
        prov.get("source_session_id").and_then(|v| v.as_str()),
        Some(legacy_sid),
        "provenance should point back at the source sid:\n{descriptor}"
    );
    remove_dir_all(&sessions).ok();
}

/// A fork-of-fork: resume an export, then resume an export built from
/// the resumed session's log. Each resume mints a new sid, and the
/// history propagates (the final session carries the original events).
/// Guards the state-accumulation boundary -- a broken seed would lose or
/// duplicate events across forks.
#[test]
fn test_fork_chain_propagates_history() {
    let sessions = temp_root();
    // Generation A: a fixture export with two events.
    let export_a = sessions.join("a.json");
    let legacy_sid = "01KZ5RDH4DG6YV0EDBX1KSKTRA";
    let src_sid = SessionId::from_display_string(legacy_sid).unwrap();
    write_export(&export_a, legacy_sid, "chain-model", &two_events(src_sid));

    // Generation B: resume A -> a new sid with A's history seeded.
    let resumed = super::build_runner_for_resume_export(
        &export_a,
        &sessions,
        None,
        None,
        ProviderResolution::stub(),
    )
    .expect("resume A -> B");
    let sid_b = resumed.assembled.session;

    let log_b = read_to_string(sessions.join(sid_b.to_string()).join("log.jsonl")).unwrap();
    assert!(
        log_b.contains("resume-unit-prompt"),
        "B should carry A's history:\n{log_b}"
    );

    // Build export B from B's durable log (what /export does at runtime):
    // each line is a SessionLogEntry; repackage with B's sid.
    let events_b: Vec<SessionLogEntry> = log_b
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).expect("log line deserializes"))
        .collect();
    let export_b = sessions.join("b.json");
    write_export(&export_b, &sid_b.to_string(), "chain-model", &events_b);

    // Generation C: resume B -> a new sid with B's (=A's) history seeded.
    let resumed = super::build_runner_for_resume_export(
        &export_b,
        &sessions,
        None,
        None,
        ProviderResolution::stub(),
    )
    .expect("resume B -> C");
    let sid_c = resumed.assembled.session;

    assert_ne!(sid_c, sid_b, "C is a new sid, not B");
    let log_c = read_to_string(sessions.join(sid_c.to_string()).join("log.jsonl")).unwrap();
    assert!(
        log_c.contains("resume-unit-prompt"),
        "C should carry the propagated history:\n{log_c}"
    );
    remove_dir_all(&sessions).ok();
}

/// build_runner_for_resume_sid re-opens an existing session: the log stays,
/// the model is restored from the descriptor, and a missing sid errors.
#[test]
fn test_resume_sid_reopens_errors() {
    let sessions = temp_root();
    // Seed a session via the export path, then re-open it by sid.
    let export_path = sessions.join("export.json");
    let legacy_sid = "01KZ5RDH4DG6YV0EDBX1KSKTRA";
    let src_sid = SessionId::from_display_string(legacy_sid).unwrap();
    write_export(
        &export_path,
        legacy_sid,
        "sid-resume-model",
        &two_events(src_sid),
    );
    let resumed = super::build_runner_for_resume_export(
        &export_path,
        &sessions,
        None,
        None,
        ProviderResolution::stub(),
    )
    .expect("seed via export");
    let existing_sid = resumed.assembled.session;

    // Re-open by sid: the same log + model restored from the descriptor.
    let resumed = super::build_runner_for_resume_sid(
        existing_sid,
        &sessions,
        None,
        None,
        ProviderResolution::stub(),
    )
    .expect("resume sid reopens");
    let runner = resumed.assembled.runner;
    let sid = resumed.assembled.session;
    let model = resumed.model;

    assert_eq!(sid, existing_sid, "sid resume reuses the same sid");
    assert_eq!(model, "sid-resume-model", "model restored from descriptor");
    let rt = Builder::new_current_thread().enable_all().build().unwrap();
    let events = rt.block_on(async { runner.store().replay(sid).await.unwrap() });
    assert!(events.len() >= 2, "reopened log replays the seeded events");

    // A sid with no log on disk errors (no silent fresh session).
    let missing = SessionId::new();
    let err = super::build_runner_for_resume_sid(
        missing,
        &sessions,
        None,
        None,
        ProviderResolution::stub(),
    )
    .err()
    .expect("missing sid errors");
    assert!(
        err.to_string().contains("no session log"),
        "missing sid should report no session log: {err}"
    );
    remove_dir_all(&sessions).ok();
}

/// A tampered session log: sid-resume warns (Unverified chain) but still
/// proceeds -- the log is the source of truth, the chain verdict is
/// best-effort + advisory. Covers the warn branch.
#[test]
fn test_resume_sid_warns_unverified() {
    let sessions = temp_root();
    let export_path = sessions.join("export.json");
    let legacy_sid = "01KZ5RDH4DG6YV0EDBX1KSKTRA";
    let src_sid = SessionId::from_display_string(legacy_sid).unwrap();
    write_export(
        &export_path,
        legacy_sid,
        "tamper-model",
        &two_events(src_sid),
    );
    let resumed = super::build_runner_for_resume_export(
        &export_path,
        &sessions,
        None,
        None,
        ProviderResolution::stub(),
    )
    .expect("seed via export");
    let existing_sid = resumed.assembled.session;

    // Tamper the first line so the chain breaks.
    let log = sessions.join(existing_sid.to_string()).join("log.jsonl");
    let body = read_to_string(&log).unwrap();
    write(&log, body.replacen("resume-unit-prompt", "TAMPERED", 1)).unwrap();
    // Resume still succeeds (warns to stderr, proceeds).
    let resumed = super::build_runner_for_resume_sid(
        existing_sid,
        &sessions,
        None,
        None,
        ProviderResolution::stub(),
    )
    .expect("resume proceeds despite unverified chain");
    let sid = resumed.assembled.session;

    assert_eq!(sid, existing_sid);
    remove_dir_all(&sessions).ok();
}

/// A session whose descriptor is missing (e.g. created before the descriptor
/// landed, or the file was deleted) still resumes: the model falls back
/// to the current config resolve_model, so /status shows the resolved
/// model instead of the original. The log is the source of truth; the
/// descriptor is advisory.
#[test]
fn test_resume_sid_missing_descriptor() {
    let sessions = temp_root();
    let export_path = sessions.join("export.json");
    let legacy_sid = "01KZ5RDH4DG6YV0EDBX1KSKTRA";
    let src_sid = SessionId::from_display_string(legacy_sid).unwrap();
    write_export(
        &export_path,
        legacy_sid,
        "descriptor-model",
        &two_events(src_sid),
    );
    let resumed = super::build_runner_for_resume_export(
        &export_path,
        &sessions,
        None,
        None,
        ProviderResolution::stub(),
    )
    .expect("seed via export");
    let existing_sid = resumed.assembled.session;

    // Remove the descriptor so the sid-resume path cannot restore the model.
    let descriptor = sessions.join(existing_sid.to_string()).join("session.json");
    assert!(descriptor.exists(), "descriptor should exist after seed");
    remove_file(&descriptor).expect("remove descriptor");
    // Resume still succeeds; the model falls back to resolve_model.
    let resumed = super::build_runner_for_resume_sid(
        existing_sid,
        &sessions,
        None,
        None,
        ProviderResolution::stub(),
    )
    .expect("resume succeeds without descriptor");
    let sid = resumed.assembled.session;
    let model = resumed.model;

    assert_eq!(sid, existing_sid);
    assert_eq!(
        model,
        houyicoder_config::resolve_model(),
        "model falls back to the current config without the descriptor"
    );
    remove_dir_all(&sessions).ok();
}

/// A truncated export file (a half-written JSON, as if the writer was
/// killed mid-flush) surfaces a clean error, not a panic. The resume path
/// deserializes the export; a malformed body fails serde + returns Err.
/// Guards against a crash on a corrupt input the user might hand it.
#[test]
fn test_resume_export_truncated_errors() {
    let sessions = temp_root();
    let export_path = sessions.join("truncated.json");
    // A valid export prefix cut mid-trajectory: the JSON is incomplete.
    write(
        &export_path,
        r#"{"session_id":"01KZ5RDH4DG6YV0EDBX1KSKTRA","model":"m","trajectory":[{"id":"#,
    )
    .unwrap();
    let result = super::build_runner_for_resume_export(
        &export_path,
        &sessions,
        None,
        None,
        ProviderResolution::stub(),
    );
    assert!(
        result.is_err(),
        "a truncated export must surface an error, not proceed"
    );
    // No session sid dir was created (the failure happened before seed).
    // The export file itself sits in the root; only count subdirectories
    // (a minted sid would be a UUID-named directory).
    let sid_dirs: Vec<_> = read_dir(&sessions)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect()
        })
        .unwrap_or_default();
    assert!(
        sid_dirs.is_empty(),
        "no session dir should be minted on a failed resume: {sid_dirs:?}"
    );
    remove_dir_all(&sessions).ok();
}

/// An export with an empty trajectory (session_id + model but zero
/// events) is rejected: the resume path returns an Empty error rather
/// than minting a fresh session with no history. Resuming nothing is
/// meaningless, so the boundary errors cleanly instead of producing an
/// empty session. Guards the zero-event boundary the seed loop + the
/// caller must agree on.
#[test]
fn test_export_empty_trajectory_errors() {
    let sessions = temp_root();
    let export_path = sessions.join("empty.json");
    write_export(
        &export_path,
        "01KZ5RDH4DG6YV0EDBX1KSKTRA",
        "empty-model",
        &[],
    );
    let result = super::build_runner_for_resume_export(
        &export_path,
        &sessions,
        None,
        None,
        ProviderResolution::stub(),
    );
    assert!(
        result.is_err(),
        "an empty-trajectory export must error, not mint a fresh session"
    );
    // No sid dir minted on the rejection.
    let sid_dirs: Vec<_> = read_dir(&sessions)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect()
        })
        .unwrap_or_default();
    assert!(
        sid_dirs.is_empty(),
        "no session dir on empty-export reject: {sid_dirs:?}"
    );
    remove_dir_all(&sessions).ok();
}

/// A garbage export (not JSON at all) surfaces a clean error. Guards the
/// non-JSON input boundary (a user pointing --resume at a stray file).
#[test]
fn test_resume_export_garbage_errors() {
    let sessions = temp_root();
    let export_path = sessions.join("garbage.txt");
    write(&export_path, "this is not json, sorry").unwrap();
    let result = super::build_runner_for_resume_export(
        &export_path,
        &sessions,
        None,
        None,
        ProviderResolution::stub(),
    );
    assert!(result.is_err(), "a non-JSON export must error, not proceed");
    remove_dir_all(&sessions).ok();
}

/// build_runner_for_fork mints a new sid seeded from the source's durable
/// events: the new sid differs from the source, the new log carries the
/// source's history, the source's log is untouched, and the descriptor records
/// ForkedFrom provenance pointing back at the source. Guards the
/// non-destructive fork the --fork-session flag promises.
#[test]
fn test_fork_keeps_source_untouched() {
    let sessions = temp_root();
    // Seed a source session via the export path.
    let export_path = sessions.join("export.json");
    let legacy_sid = "01KZ5RDH4DG6YV0EDBX1KSKTRA";
    let src_sid = SessionId::from_display_string(legacy_sid).unwrap();
    write_export(&export_path, legacy_sid, "fork-model", &two_events(src_sid));
    let resumed = super::build_runner_for_resume_export(
        &export_path,
        &sessions,
        None,
        None,
        ProviderResolution::stub(),
    )
    .expect("seed source via export");
    let source_sid = resumed.assembled.session;

    let source_log =
        read_to_string(sessions.join(source_sid.to_string()).join("log.jsonl")).unwrap();

    // Fork the source: a new sid seeded with the source's durable events.
    let resumed = super::build_runner_for_fork(
        source_sid,
        &sessions,
        None,
        None,
        ProviderResolution::stub(),
    )
    .expect("fork seeds new session");
    let runner = resumed.assembled.runner;
    let forked_sid = resumed.assembled.session;
    let model = resumed.model;

    assert_ne!(forked_sid, source_sid, "fork mints a new sid");
    assert_eq!(model, "fork-model", "fork restores the source's model");
    let forked_log =
        read_to_string(sessions.join(forked_sid.to_string()).join("log.jsonl")).unwrap();
    assert!(
        forked_log.contains("resume-unit-prompt"),
        "forked log carries the source history:\n{forked_log}"
    );
    // The source log is untouched (the fork did not append to it).
    assert_eq!(
        read_to_string(sessions.join(source_sid.to_string()).join("log.jsonl")).unwrap(),
        source_log,
        "fork must not touch the source log"
    );
    // The forked descriptor records ForkedFrom provenance.
    let descriptor =
        read_to_string(sessions.join(forked_sid.to_string()).join("session.json")).unwrap();
    let descriptor: serde_json::Value =
        serde_json::from_str(&descriptor).expect("descriptor is json");
    let prov = descriptor.get("provenance").and_then(|v| v.as_object());
    assert_eq!(
        prov.and_then(|o| o.get("kind")).and_then(|v| v.as_str()),
        Some("forked_from"),
        "fork provenance kind:\n{descriptor}"
    );
    assert_eq!(
        prov.and_then(|o| o.get("from_sid"))
            .and_then(|v| v.as_str()),
        Some(source_sid.to_string().as_str()),
        "fork provenance points back at the source:\n{descriptor}"
    );
    // The runner's store replays the seeded events.
    let rt = Builder::new_current_thread().enable_all().build().unwrap();
    let events = rt.block_on(async { runner.store().replay(forked_sid).await.unwrap() });
    assert!(events.len() >= 2, "forked store replays the seeded events");
    // The fork fills the log state before assembly too: same seeding contract.
    assert_eq!(
        runner.extractor_cursor(),
        events.last().map(|e| e.id),
        "the forked tail seeds the extractor cursor"
    );
    remove_dir_all(&sessions).ok();
}

/// Forking a sid with no log on disk errors cleanly (the existence precheck)
/// rather than minting an empty fork or surfacing a cryptic replay error.
/// Guards the no-source boundary + matches resume_sid's friendly error.
#[test]
fn test_fork_empty_source_errors() {
    let sessions = temp_root();
    let empty_sid = SessionId::new();
    let result =
        super::build_runner_for_fork(empty_sid, &sessions, None, None, ProviderResolution::stub());
    let err = result
        .err()
        .expect("forking a sid with no log must error, not mint an empty session");
    assert!(
        err.to_string().contains("no session log"),
        "missing source sid should report no session log: {err}"
    );
    remove_dir_all(&sessions).ok();
}

/// Forking a session whose source log is tampered warns (Unverified chain)
/// but still proceeds -- the fork's chain is rebuilt on the durable subset,
/// so it is internally self-consistent; the warn surfaces that the source was
/// tampered/corrupt so the user knows the fork's lineage is questionable.
/// Mirrors resume_sid_warns_on_unverified. (The warn goes to stderr; the test
/// asserts the fork proceeds, not the stderr text -- stderr is not captured.)
#[test]
fn test_fork_warns_unverified_source() {
    let sessions = temp_root();
    let export_path = sessions.join("export.json");
    let legacy_sid = "01KZ5RDH4DG6YV0EDBX1KSKTRA";
    let src_sid = SessionId::from_display_string(legacy_sid).unwrap();
    write_export(
        &export_path,
        legacy_sid,
        "tamper-model",
        &two_events(src_sid),
    );
    let resumed = super::build_runner_for_resume_export(
        &export_path,
        &sessions,
        None,
        None,
        ProviderResolution::stub(),
    )
    .expect("seed source via export");
    let source_sid = resumed.assembled.session;

    // Tamper the first event so the source chain breaks.
    let log = sessions.join(source_sid.to_string()).join("log.jsonl");
    let body = read_to_string(&log).unwrap();
    write(&log, body.replacen("resume-unit-prompt", "TAMPERED", 1)).unwrap();
    // Fork still succeeds (warns to stderr, proceeds).
    let resumed = super::build_runner_for_fork(
        source_sid,
        &sessions,
        None,
        None,
        ProviderResolution::stub(),
    )
    .expect("fork proceeds despite unverified source chain");
    let forked_sid = resumed.assembled.session;

    assert_ne!(forked_sid, source_sid, "fork mints a new sid");
    remove_dir_all(&sessions).ok();
}

/// latest_session_sid returns the active in-cwd session (max log.jsonl mtime,
/// excludes no-log + other-cwd sessions). None when no session matches. Guards
/// the --continue pick.
/// --continue resolution path.
#[test]
fn test_latest_picks_active_cwd() {
    let sessions = temp_root();
    // No sessions -> None.
    assert!(super::latest_session_sid(&sessions).is_none());

    // A: an active session in the test workspace (seeded via export -> has a
    // log + descriptor, cwd = workspace_cwd(None) the same way --continue sees).
    let export_a = sessions.join("a.json");
    let sid_a = SessionId::new();
    write_export(&export_a, &sid_a.to_string(), "ma", &two_events(sid_a));
    let resumed = super::build_runner_for_resume_export(
        &export_a,
        &sessions,
        None,
        None,
        ProviderResolution::stub(),
    )
    .unwrap();
    let minted_a = resumed.assembled.session;

    // B: a descriptor-only session in the SAME workspace but with no durable log
    // (zero turns -- e.g. a session opened + immediately quit). --continue
    // must exclude it: "continue" presupposes something to continue.
    let sid_b = SessionId::new();
    let descriptor_store: Arc<dyn houyicoder_context::SessionDescriptorStore> =
        Arc::new(FileDescriptorStore::new(sessions.clone()));
    let b_descriptor = houyicoder_context::SessionDescriptor {
        name: None,
        name_source: NameSource::Auto,
        cwd: workspace_cwd(None),
        model: "mb".into(),
        provenance: SessionProvenance::Fresh,
        version: "t".into(),
        created_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        child_session_ids: Vec::new(),
    };
    descriptor_store
        .write_descriptor(sid_b, &b_descriptor)
        .unwrap();
    // B has no log.jsonl (never appended).

    // C: an active session in a DIFFERENT workspace (project pins its cwd
    // elsewhere). --continue must not jump across workspaces.
    let other_ws = temp_dir().join(format!("other-ws-{}-c", id()));
    create_dir_all(&other_ws).unwrap();
    let export_c = sessions.join("c.json");
    let sid_c = SessionId::new();
    write_export(&export_c, &sid_c.to_string(), "mc", &two_events(sid_c));
    let resumed = super::build_runner_for_resume_export(
        &export_c,
        &sessions,
        Some(other_ws.to_string_lossy().into_owned()),
        None,
        ProviderResolution::stub(),
    )
    .unwrap();
    let minted_c = resumed.assembled.session;

    // --continue picks A: B is excluded (no log), C is excluded (other cwd).
    let latest = super::latest_session_sid(&sessions).expect("A is pickable");
    assert_eq!(
        latest, minted_a,
        "latest_session_sid picks the active in-cwd session, not the no-log or other-cwd ones"
    );
    assert_ne!(latest, minted_c, "must not pick the other-cwd session");
    remove_dir_all(&sessions).ok();
    remove_dir_all(&other_ws).ok();
}

mod select;
