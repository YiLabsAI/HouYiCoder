//! On-disk session seeding helpers for the PTY tests: write the session log
//! and sidecar (and a checkpoint manifest for the post-compact journey) into
//! a temp sessions dir the binary will read on --resume. Split from mod.rs on
//! the file-size gate. Each helper is self-contained (external-crate imports
//! only); the read path in LocalFileBackend scans the same layout these write,
//! so the binary's resume + /context see the seeded state.

/// Seed a session on disk (a sid log file with one UserInput event + a
/// session.json sidecar carrying the model) so a subsequent --resume <sid>
/// or the /resume picker can re-open it. Shared by the sid-resume + lock
/// PTY tests.
pub fn seed_session_on_disk(root: &std::path::Path, sid_str: &str, model: &str, prompt: &str) {
    let cwd = houyicoder_service::composition::workspace_cwd(None);
    seed_session_with_cwd(root, sid_str, model, prompt, &cwd);
}

/// Seed a session with an EXPLICIT recorded cwd (not the current workspace).
/// Used by the --continue cwd-convergence test to plant an other-workspace
/// session the cwd filter must reject -- a session that would be picked if
/// the filter were absent, so its presence discriminates the filter from a
/// no-session empty case. Returns the seeded event's id so a caller that
/// also writes a checkpoint manifest (seed_session_with_checkpoint) can
/// reference it as last_event + the Summarized turn without re-deriving or
/// duplicating the log+sidecar write.
pub fn seed_session_with_cwd(
    root: &std::path::Path,
    sid_str: &str,
    model: &str,
    prompt: &str,
    cwd: &str,
) -> houyicoder_core::EventId {
    use houyicoder_core::{EventId, SessionId, TurnEvent, TurnEventKind};
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
        "cwd": cwd,
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
    event.id
}

/// Seed a session sidecar (session.json) with NO log and NO name/prompt, so the
/// /resume picker sees an "empty" session whose title falls back to the
/// disambiguating placeholder. Used by the picker-disambiguation PTY test to
/// create several tellable-apart empty sessions.
pub fn seed_meta_only(root: &std::path::Path, sid_str: &str) {
    let dir = root.join(sid_str);
    std::fs::create_dir_all(&dir).expect("mkdir session dir");
    let meta = serde_json::json!({
        "name": null,
        "name_source": "auto",
        "cwd": houyicoder_service::composition::workspace_cwd(None),
        "model": "meta-only",
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

/// Seed a session on disk WITH a checkpoint manifest — a post-compact state —
/// so a launched --resume <sid> followed by /context shows the folded
/// summary + the Compact buffer category. Writes the log (one UserInput
/// event), the session.json sidecar, and a CheckpointManifest via the real
/// LocalFileBackend (not hand-rolled JSON: the manifest schema can drift, and
/// the backend's read path would fail to deserialize a hand-written file).
/// The event is the manifest's last_event + the single Summarized turn, so
/// compact_summary returns a non-None line + the dispatch injects a
/// "Compact buffer" category. For the PTY /context post-compact journey.
///
/// write_checkpoint is a ContextBackend trait method returning a PFut
/// (internally synchronous, then wrapped); the helper is a sync fn called from
/// non-async PTY tests, so it blocks on the future directly rather than
/// spinning a tokio runtime.
pub fn seed_session_with_checkpoint(
    root: &std::path::Path,
    sid_str: &str,
    model: &str,
    prompt: &str,
    summary: &str,
) {
    use houyicoder_context::{
        CheckpointId, CheckpointManifest, Disposition, SessionId, TurnGroup,
        backend::ContextBackend,
    };
    // Reuse seed_session_with_cwd for the log + sidecar write + the event id,
    // so the two seed paths share one source of truth (a schema change to the
    // event or sidecar shape only has to be fixed once, not in two copies).
    let cwd = houyicoder_service::composition::workspace_cwd(None);
    let event_id = seed_session_with_cwd(root, sid_str, model, prompt, &cwd);
    let sid = SessionId::from_display_string(sid_str).expect("sid parses");
    let manifest = CheckpointManifest {
        id: CheckpointId::new(),
        session: sid,
        last_event: event_id,
        summary: Some(summary.into()),
        plan: vec![TurnGroup {
            turn_id: event_id,
            disposition: Disposition::Summarized,
            event_ids: vec![event_id],
        }],
        ts: 0,
    };
    let backend = houyicoder_memory::LocalFileBackend::new(root.to_path_buf());
    futures::executor::block_on(backend.write_checkpoint(manifest)).expect("write checkpoint");
}
