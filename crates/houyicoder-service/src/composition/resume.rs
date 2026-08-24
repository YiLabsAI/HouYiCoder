//! Resume entry for the composition root: build a runner whose session log +
//! trajectory are seeded from an exported transcript file. Split out of the
//! composition module on size grounds (same pattern as the memory + worktree
//! + session_meta submodules). The CLI --resume <file> branch lands here.

use super::*;
use houyicoder_context::TurnEvent;
use houyicoder_context::{NameSource, SessionMeta, SessionMetaStore, SessionProvenance};
use houyicoder_memory::{FileMetaStore, LocalFileBackend};
use houyicoder_session::{SessionStore, SourceChain};
use std::path::Path;

/// The most-recently-active session id in the current workspace, or None when
/// no session with a durable log matches. Drives --continue: the session is
/// picked by log.jsonl mtime (last append = last activity), the only signal
/// that reflects real usage. Sessions without a log (zero durable events) are
/// excluded -- "continue" presupposes something to continue, and resuming an
/// empty session is a no-op. Converges strictly to the current workspace
/// (meta.cwd match); no cross-workspace fallback -- a silent jump into another
/// repo's session is the hazard cwd convergence exists to prevent.
pub fn latest_session_sid(sessions_root: &Path) -> Option<SessionId> {
    let cwd = workspace_cwd(None);
    let meta_store: Arc<dyn SessionMetaStore> =
        Arc::new(FileMetaStore::new(sessions_root.to_path_buf()));
    // Stat-first: take the 200 most recently active sessions WITH a log
    // (stat only, no sidecar parse), then parse only those for cwd match.
    // On a 50k backlog this replaces 50k JSON parses with 200. A session
    // without a log is skipped at the stat phase -- --continue needs
    // something to continue, and resume_sid hard-errors on a missing log.
    let recent = crate::session_prune::list_recent_sessions(sessions_root, 200);
    let found = recent
        .iter()
        .filter_map(|(sid, _)| {
            let m = meta_store.read_meta(*sid)?;
            (m.cwd == cwd).then_some((sid, ()))
        })
        .map(|(sid, _)| *sid)
        .next();
    if found.is_some() {
        return found;
    }
    // Fallback: the cwd's session is outside the top 200 (old but still
    // the only one in this cwd). Do the full scan — rare, and the 200
    // window can be raised if it fires often enough to matter.
    meta_store
        .list_metas()
        .into_iter()
        .filter(|(_, m)| m.cwd == cwd)
        .filter_map(|(sid, _)| log_last_active_secs(sessions_root, &sid).map(|secs| (sid, secs)))
        .max_by_key(|(_, secs)| *secs)
        .map(|(sid, _)| sid)
}

/// The last-append time of a session's durable log as Unix-epoch seconds, or
/// None when the session has no log.jsonl (never appended -- zero durable
/// events). One metadata() call gives both existence and mtime; missing log
/// means "never appended" (the same convention log_size documents).
pub fn log_last_active_secs(sessions_root: &Path, sid: &SessionId) -> Option<u64> {
    let path = sessions_root.join(sid.to_string()).join("log.jsonl");
    std::fs::metadata(&path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

/// The minimal slice of an export document the resume path consumes. The
/// full ExportData struct (in the CLI export bridge) carries derived stats
/// (tool_stats, usage, checkpoints, errors) the resume path does not need;
/// deserializing into this slice lets serde ignore the extra fields, so a
/// resume never breaks when the export format adds a derived field later.
#[derive(serde::Deserialize)]
struct ResumePayload {
    session_id: String,
    model: String,
    trajectory: Vec<TurnEvent>,
}

/// Build a runner resumed from an exported transcript file (--resume <file>
/// when the value is an existing path). The export is a snapshot, not a live
/// log, so resume forks a new session: a fresh session id is minted, the
/// export's durable events are seeded into the new session's log + the
/// trajectory mirror (delta events dropped, the hash chain rebuilt on the
/// durable subset), and the sidecar records the lineage (provenance =
/// ResumedFromExport). The model from the export is restored (the runner
/// runs with the model the export ran with). Returns the 6-tuple the TUI
/// wiring expects (runner, session, model, gate, sandbox, notify).
pub fn build_runner_for_resume_export(
    export_path: &Path,
    sessions_root: &Path,
    project: Option<String>,
    rule_store: Option<Arc<dyn RuleStore>>,
) -> Result<ResumedRunner, ResumeError> {
    let body = std::fs::read_to_string(export_path)
        .map_err(|e| ResumeError::Read(format!("{}: {e}", export_path.display())))?;
    let payload: ResumePayload = serde_json::from_str(&body)
        .map_err(|e| ResumeError::Parse(format!("{}: {e}", export_path.display())))?;
    if payload.trajectory.is_empty() {
        return Err(ResumeError::Empty(export_path.display().to_string()));
    }
    let append_notify = Arc::new(Notify::new());
    // Disk backend at the sid-keyed sessions root so the seeded durable
    // events land on disk under the new sid (the resume foundation: the
    // next --resume <sid> can pick this session back up). Always disk here
    // regardless of cfg(test) -- the resume path is a prod entry, and the
    // PTY test that exercises it isolates the root via the sessions-dir env.
    let backend = LocalFileBackend::new(sessions_root.to_path_buf());
    let store =
        Arc::new(SessionStore::new(Box::new(backend)).with_append_notify(append_notify.clone()));
    let session = SessionId::new();
    // Seed the trajectory: verify the source chain (with deltas) then
    // rebuild the durable subset onto the new session (deltas dropped, the
    // new chain is internally self-consistent). seed_trajectory is async
    // (the backend append facade is); block on it with a one-shot runtime.
    let seed_store = Arc::clone(&store);
    let seed_session = session;
    let events = payload.trajectory;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| ResumeError::Seed(format!("runtime: {e}")))?;
    let report = rt.block_on(async move {
        seed_store
            .seed_trajectory(seed_session, events)
            .await
            .map_err(|e| ResumeError::Seed(format!("{e}")))
    })?;
    if report.durable_count == 0 {
        return Err(ResumeError::Seed(
            "no durable events after seeding (all deltas?)".into(),
        ));
    }
    // Write the sidecar with the resume lineage so /status can show it +
    // a later resume can carry it forward. Best-effort (see write_initial).
    let meta_store: Arc<dyn SessionMetaStore> =
        Arc::new(FileMetaStore::new(sessions_root.to_path_buf()));
    write_resume_session_meta(
        &meta_store,
        session,
        &payload.model,
        project.as_deref(),
        &payload.session_id,
    );
    let model = payload.model;
    let model_for_return = model.clone();
    let assembled = assemble(store, session, model, project, rule_store, append_notify);
    Ok(ResumedRunner {
        assembled,
        model: model_for_return,
    })
}

/// Build a runner resumed from a session already on disk (--resume <sid>).
/// The session's log.jsonl + session.json exist under the sid-keyed sessions
/// root; this re-opens them (the engine reads the history via backend replay
/// on the next run, and last_hashes self-recovers via the cold reverse-read
/// of the last disk line so new appends chain correctly). No seed -- the log
/// is already there. The model is restored from the session.json sidecar
/// (fallback to the current config when the sidecar is missing or its model
/// is empty). Returns the 6-tuple the TUI wiring expects.
pub fn build_runner_for_resume_sid(
    sid: SessionId,
    sessions_root: &Path,
    project: Option<String>,
    rule_store: Option<Arc<dyn RuleStore>>,
) -> Result<ResumedRunner, ResumeError> {
    let log_path = sessions_root.join(format!("{sid}")).join("log.jsonl");
    if !log_path.exists() {
        return Err(ResumeError::Read(format!(
            "no session log at {} (is the sid a session id?)",
            log_path.display()
        )));
    }
    let append_notify = Arc::new(Notify::new());
    let backend = LocalFileBackend::new(sessions_root.to_path_buf());
    let store =
        Arc::new(SessionStore::new(Box::new(backend)).with_append_notify(append_notify.clone()));
    // Best-effort chain verify: hash the raw on-disk line bytes (not a
    // re-serialization) so a cross-binary log with a serde-default schema
    // drift still verifies. A tampered or broken chain warns but does not
    // block resume (a crashed session may still recover); /status shows the
    // verdict.
    match store.verify_disk_chain(sid) {
        SourceChain::Verified => {}
        SourceChain::Unverified { at_index, reason } => tracing::warn!(
            "resume: session {sid} chain unverified at event {at_index}: {reason}; \
             resuming anyway (the log is the source of truth)"
        ),
    }
    // Backfill the in-memory trajectory mirror from the durable log so the
    // serve-start replay ships the resumed history to the client (the working
    // screen shows the past turns, not just the status bar). Best-effort: a
    // read failure falls through to an empty mirror (the run still works, the
    // history just is not pre-rendered).
    let restore_store = Arc::clone(&store);
    let restore_session = sid;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| ResumeError::Read(format!("restore runtime: {e}")))?;
    let restore_result =
        rt.block_on(async move { restore_store.restore_trajectory(restore_session).await });
    drop(rt);
    if let Err(e) = &restore_result {
        tracing::warn!(
            "resume: history load failed for {sid}: {e}; history will not \
             pre-render (the run still works)"
        );
    }
    // Restore the model from the sidecar; fall back to the current config so
    // a session whose sidecar is missing (created before the sidecar landed)
    // still resumes -- /status will show the resolved model instead.
    let meta_store: Arc<dyn SessionMetaStore> =
        Arc::new(FileMetaStore::new(sessions_root.to_path_buf()));
    let model = meta_store
        .read_meta(sid)
        .and_then(|m| {
            if m.model.is_empty() {
                None
            } else {
                Some(m.model)
            }
        })
        .unwrap_or_else(houyicoder_config::resolve_model);
    let model_for_return = model.clone();
    let assembled = assemble(store, sid, model, project, rule_store, append_notify);
    Ok(ResumedRunner {
        assembled,
        model: model_for_return,
    })
}

/// Build a runner that forks an existing session (--resume <sid> --fork-session
/// or --continue --fork-session). Mints a fresh session id, seeds its log with
/// the source session's durable events (a snapshot at the fork point — the
/// source is untouched), and records ForkedFrom provenance so /status can show
/// the lineage. The new sid is unique, so no other process holds it; the
/// source's lock is not acquired. The model is restored from the source's
/// sidecar (fallback to the current config when missing or empty).
pub fn build_runner_for_fork(
    source_sid: SessionId,
    sessions_root: &Path,
    project: Option<String>,
    rule_store: Option<Arc<dyn RuleStore>>,
) -> Result<ResumedRunner, ResumeError> {
    let append_notify = Arc::new(Notify::new());
    let backend = LocalFileBackend::new(sessions_root.to_path_buf());
    let store =
        Arc::new(SessionStore::new(Box::new(backend)).with_append_notify(append_notify.clone()));
    let new_session = SessionId::new();
    // Existence precheck: a sid with no log on disk gets a friendly error
    // (mirrors resume_sid), not a cryptic "source replay" / Empty later.
    let log_path = sessions_root
        .join(format!("{source_sid}"))
        .join("log.jsonl");
    if !log_path.exists() {
        return Err(ResumeError::Read(format!(
            "no session log at {} (is the sid a session id?)",
            log_path.display()
        )));
    }
    // Verify the source chain before seeding: seed_trajectory rebuilds a
    // self-consistent chain on the durable subset, so a fork from a tampered
    // or corrupted source would otherwise launder into a Verified new session.
    // Warn (do not block) -- the log is the source of truth, a crashed session
    // may still recover, and the warn surfaces the lineage question to /status.
    match store.verify_disk_chain(source_sid) {
        SourceChain::Verified => {}
        SourceChain::Unverified { at_index, reason } => tracing::warn!(
            "fork: source session {source_sid} chain unverified at event {at_index}: {reason}; \
             forking anyway (the fork's chain is rebuilt, but the source was tampered/corrupt)"
        ),
    }
    // Read the source's durable events, then seed them onto the new sid (the
    // seed rebuilds the chain on the durable subset, so the fork's chain is
    // internally consistent + independent of the source's). seed_trajectory is
    // async (the backend append facade is); block on it with a one-shot runtime.
    let seed_store = Arc::clone(&store);
    let seed_session = new_session;
    let from_sid_str = source_sid.to_string();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| ResumeError::Seed(format!("runtime: {e}")))?;
    let report = rt.block_on(async move {
        let events = seed_store
            .backend()
            .replay(source_sid)
            .await
            .map_err(|e| ResumeError::Read(format!("source replay: {e}")))?;
        if events.is_empty() {
            return Err(ResumeError::Empty(from_sid_str));
        }
        seed_store
            .seed_trajectory(seed_session, events)
            .await
            .map_err(|e| ResumeError::Seed(format!("{e}")))
    })?;
    drop(rt);
    if report.durable_count == 0 {
        return Err(ResumeError::Seed(
            "no durable events after forking (all deltas?)".into(),
        ));
    }
    let meta_store: Arc<dyn SessionMetaStore> =
        Arc::new(FileMetaStore::new(sessions_root.to_path_buf()));
    let model = meta_store
        .read_meta(source_sid)
        .and_then(|m| {
            if m.model.is_empty() {
                None
            } else {
                Some(m.model)
            }
        })
        .unwrap_or_else(houyicoder_config::resolve_model);
    write_fork_session_meta(
        &meta_store,
        new_session,
        &model,
        project.as_deref(),
        &source_sid.to_string(),
        report.durable_count as u64,
    );
    let model_for_return = model.clone();
    let assembled = assemble(
        store,
        new_session,
        model,
        project,
        rule_store,
        append_notify,
    );
    Ok(ResumedRunner {
        assembled,
        model: model_for_return,
    })
}

/// A resume failure: the export could not be read, parsed, was empty, or the
/// seed failed. Distinct from a generic Box<dyn Error> so the CLI can surface
/// the stage that failed (read vs parse vs seed) without re-parsing.
#[derive(Debug)]
pub enum ResumeError {
    Read(String),
    Parse(String),
    Empty(String),
    Seed(String),
}

impl std::fmt::Display for ResumeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read(m) => write!(f, "resume read failed: {m}"),
            Self::Parse(m) => write!(f, "resume parse failed: {m}"),
            Self::Empty(m) => write!(f, "resume export has no events: {m}"),
            Self::Seed(m) => write!(f, "resume seed failed: {m}"),
        }
    }
}

impl std::error::Error for ResumeError {}

/// Write the sidecar for a resumed session. Provenance is ResumedFromExport
/// (carries the source session id forward); cwd + model come from the export;
/// name starts None (auto-derived from the seeded first prompt at display
/// time). Best-effort: a sidecar write failure surfaces on stderr but does
/// not block the resume -- the engine runs without it.
fn write_resume_session_meta(
    meta_store: &Arc<dyn SessionMetaStore>,
    session: SessionId,
    model: &str,
    project: Option<&str>,
    source_session_id: &str,
) {
    let cwd = workspace_cwd(project.map(str::to_string));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let meta = SessionMeta {
        name: None,
        name_source: NameSource::Auto,
        cwd,
        model: model.to_string(),
        provenance: SessionProvenance::ResumedFromExport {
            source_session_id: source_session_id.to_string(),
        },
        version: env!("CARGO_PKG_VERSION").to_string(),
        created_at: now,
        child_session_ids: Vec::new(),
    };
    if let Err(e) = meta_store.write_meta(session, &meta) {
        tracing::warn!("session meta: resume write failed: {e}; /status will show less");
    }
}

/// Write the sidecar for a forked session. Provenance is ForkedFrom (carries
/// the source sid + the event count at the fork point); cwd + model come from
/// the current invocation + the source's model; name starts None (auto-derived
/// at display time). Best-effort: a sidecar write failure surfaces on stderr
/// but does not block the fork.
fn write_fork_session_meta(
    meta_store: &Arc<dyn SessionMetaStore>,
    session: SessionId,
    model: &str,
    project: Option<&str>,
    from_sid: &str,
    from_seq: u64,
) {
    let cwd = workspace_cwd(project.map(str::to_string));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let meta = SessionMeta {
        name: None,
        name_source: NameSource::Auto,
        cwd,
        model: model.to_string(),
        provenance: SessionProvenance::ForkedFrom {
            from_sid: from_sid.to_string(),
            from_seq: Some(from_seq),
        },
        version: env!("CARGO_PKG_VERSION").to_string(),
        created_at: now,
        child_session_ids: Vec::new(),
    };
    if let Err(e) = meta_store.write_meta(session, &meta) {
        tracing::warn!("session meta: fork write failed: {e}; /status will show less");
    }
}

#[cfg(test)]
#[path = "resume_tests.rs"]
mod tests;
