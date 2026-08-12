//! The CLI-side bridge for the TUI session picker: implements the TUI's
//! SessionLister trait over the sidecar store (FileMetaStore) + the runner's
//! SessionLog. The TUI names SessionLister; this bridge provides it, so the
//! TUI stays a presentation layer and never imports the storage traits (the
//! dep-graph layering). Title derivation: the sidecar name wins,
//! else the first user prompt slugified from the session log head,
//! else a placeholder.

use std::sync::Arc;

use houyicoder_api::session::SessionLog;
use houyicoder_context::{SessionId, SessionMetaStore, TurnEvent, TurnEventKind};
use houyicoder_tui::resume_picker::{SessionLister, SessionRow};

pub struct SessionListerBridge {
    meta_store: Arc<dyn SessionMetaStore>,
    session_log: Arc<dyn SessionLog>,
    sessions_root: std::path::PathBuf,
}

impl SessionListerBridge {
    pub fn new(
        meta_store: Arc<dyn SessionMetaStore>,
        session_log: Arc<dyn SessionLog>,
        sessions_root: std::path::PathBuf,
    ) -> Self {
        Self {
            meta_store,
            session_log,
            sessions_root,
        }
    }
}

impl SessionLister for SessionListerBridge {
    fn list_sessions(&self, current_sid: &str) -> Vec<SessionRow> {
        let current = SessionId::from_display_string(current_sid).unwrap_or_default();
        let mut rows: Vec<SessionRow> = self
            .meta_store
            .list_metas()
            .into_iter()
            .filter(|(sid, _)| *sid != current)
            .map(|(sid, meta)| {
                let cwd_basename = meta
                    .cwd
                    .rsplit('/')
                    .next()
                    .filter(|s| !s.is_empty())
                    .unwrap_or("?")
                    .to_string();
                // Cheap title: sidecar name if set, else placeholder. The
                // expensive title (first-prompt slug from a log-head READ +
                // serde parse) is filled by resolve_detail, lazily, for
                // visible rows only. list_sessions does NOT read the log.
                let title = meta
                    .name
                    .as_ref()
                    .filter(|n| !n.trim().is_empty())
                    .cloned()
                    .unwrap_or_else(|| format!("(session) {}", short_sid(sid)));
                // last_active from a log mtime STAT (one metadata() call,
                // no read/parse) so the sort reflects real activity, not just
                // creation order — a session resumed 5m ago but created
                // weeks ago must sort above one created 1h ago and idle
                // since. Falls back to the sidecar created_at when no log
                // exists. The expensive log-head read for the title stays
                // in resolve_detail.
                let last_active = houyicoder_service::composition::log_last_active_secs(
                    &self.sessions_root,
                    &sid,
                )
                .unwrap_or(meta.created_at);
                SessionRow {
                    sid_str: sid.to_string(),
                    title,
                    cwd_basename,
                    last_active,
                    ..Default::default()
                }
            })
            .collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.last_active));
        // Dedup by the cheap title: when multiple sessions share the same
        // sidecar name (the common "re-running + naming alike" case), keep
        // only the most recently active one. The sort put the newest first,
        // so the first occurrence of each title wins. Placeholder titles are
        // unique (short sid suffix), so unnamed sessions never dedup here —
        // their slug-dedup happens lazily in the picker after resolve_detail
        // fills the real title (see run_control's hidden-row pass).
        let mut seen_titles: std::collections::HashSet<String> = std::collections::HashSet::new();
        rows.retain(|r| seen_titles.insert(r.title.clone()));
        rows
    }

    fn resolve_detail(&self, row: &mut SessionRow) {
        let Some(sid) = SessionId::from_display_string(&row.sid_str) else {
            return;
        };
        // Re-read the sidecar to decide the title unambiguously: a user-set
        // name wins (even if it happens to start with "(session)", which the
        // old starts_with heuristic would have mistaken for a placeholder).
        // Only when there is no sidecar name do we pay the log-head read +
        // serde parse for the first-prompt slug. last_active is already the
        // log mtime (set by list_sessions' stat), so no re-stat here.
        let has_name = self
            .meta_store
            .read_meta(sid)
            .as_ref()
            .and_then(|m| m.name.as_ref())
            .is_some_and(|n| !n.trim().is_empty());
        if has_name {
            return;
        }
        if let Some(prompt) = first_user_prompt(self.session_log.as_ref(), sid) {
            let slug = slugify(&prompt);
            // Guard: a prompt with no alphanumeric chars (e.g. "???", pure
            // whitespace) slugifies to empty — keep the disambiguating
            // placeholder rather than show a blank row.
            if !slug.is_empty() {
                row.title = slug;
            }
        }
    }
}

/// The first 8 hex chars of a session id, for the disambiguating placeholder.
fn short_sid(sid: SessionId) -> String {
    sid.to_string().chars().take(8).collect()
}

fn first_user_prompt(session_log: &dyn SessionLog, sid: SessionId) -> Option<String> {
    let backend = session_log.backend();
    let read = backend.read_log_range(sid, 0, 64_000);
    for (_, line) in &read.lines {
        if let Ok(ev) = serde_json::from_str::<TurnEvent>(line)
            && let TurnEventKind::UserInput { text } = &ev.kind
        {
            return Some(text.clone());
        }
    }
    None
}

fn slugify(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut prev_dash = true;
    for c in text.chars().take(50) {
        if c.is_alphanumeric() {
            for lc in c.to_lowercase() {
                out.push(lc);
            }
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.chars().count() > 40 {
        out.chars().take(40).collect()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use houyicoder_context::{
        EventId, NameSource, SessionId, SessionMeta, SessionProvenance, TurnEvent, TurnEventKind,
    };
    use houyicoder_memory::{FileMetaStore, InMemoryBackend, InMemoryMetaStore};
    use houyicoder_session::SessionStore;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_root() -> std::path::PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("lister-bridge-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn meta(name: Option<&str>, cwd: &str, ts: u64) -> SessionMeta {
        SessionMeta {
            name: name.map(str::to_string),
            name_source: NameSource::Auto,
            cwd: cwd.into(),
            model: "test".into(),
            provenance: SessionProvenance::Fresh,
            version: "t".into(),
            created_at: ts,
        }
    }

    /// list_sessions is the cheap phase: sidecar read + one log-mtime stat
    /// per session (no log-head read/parse). It sorts by real last activity
    /// (log mtime, created_at fallback), excludes the current session, and
    /// uses the sidecar name or a placeholder title (the expensive first-
    /// prompt slug is resolve_detail's job, proven below).
    #[tokio::test]
    async fn test_bridge_lists_derives_titles() {
        let root = temp_root();
        let meta_store = InMemoryMetaStore::new();
        let cur = SessionId::new();
        let older = SessionId::new();
        let newer = SessionId::new();
        meta_store
            .write_meta(older, &meta(None, "/repo/a", 100))
            .unwrap();
        meta_store
            .write_meta(newer, &meta(Some("named session"), "/repo/b", 200))
            .unwrap();
        meta_store
            .write_meta(cur, &meta(None, "/repo/c", 50))
            .unwrap();
        // A disk-backed SessionStore so the bridge can read the first prompt
        // from the older session log head (InMemoryBackend has no
        // read_log_range, so the disk path is the one that exercises title
        // derivation).
        let store = SessionStore::new(Box::new(houyicoder_memory::LocalFileBackend::new(
            root.clone(),
        )));
        store
            .append(TurnEvent {
                id: EventId::new(),
                session: older,
                ts: 0,
                prev_hash: None,
                kind: TurnEventKind::UserInput {
                    text: "hello world prompt".into(),
                },
            })
            .await
            .unwrap();
        let log: Arc<dyn SessionLog> = Arc::new(store);
        let bridge = SessionListerBridge::new(Arc::new(meta_store), log, root.clone());
        let mut rows = bridge.list_sessions(&cur.to_string());
        assert_eq!(rows.len(), 2, "current session excluded: {rows:?}");
        // Phase 1 (list_sessions) sorts by log.jsonl mtime (last activity).
        // The older session has a log (just appended) so it is most-active
        // -> first; the newer session has no log (created_at fallback=200)
        // -> second despite the later created timestamp. This proves the
        // sort is by real mtime, not created_at.
        assert_eq!(
            rows[0].sid_str,
            older.to_string(),
            "session with a log sorts first by mtime"
        );
        // Phase 1 titles are CHEAP: sidecar name or a placeholder, NOT the
        // first-prompt slug (that read is deferred). The older session has
        // no sidecar name -> placeholder.
        assert!(
            rows[0].title.starts_with("(session) "),
            "list_sessions returns a placeholder, not the slug: {}",
            rows[0].title
        );
        assert_eq!(
            rows[1].sid_str,
            newer.to_string(),
            "no-log session falls back to created_at + sorts second"
        );
        assert_eq!(
            rows[1].title, "named session",
            "sidecar name is the cheap title (no log read)"
        );
        assert_eq!(rows[1].cwd_basename, "b");
        // Phase 2 (resolve_detail) fills the expensive title from the log
        // head, only for placeholder rows (sidecar name wins stays).
        bridge.resolve_detail(&mut rows[0]);
        assert_eq!(
            rows[0].title, "hello-world-prompt",
            "resolve_detail fills the first-prompt slug"
        );
        bridge.resolve_detail(&mut rows[1]);
        assert_eq!(
            rows[1].title, "named session",
            "resolve_detail leaves a sidecar name untouched"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// A session with no name + no UserInput in the log head keeps its
    /// disambiguating placeholder (short sid suffix, so several empty
    /// sessions are tellable apart) after resolve_detail — not an error
    /// (the picker must not fail to open).
    #[tokio::test]
    async fn test_bridge_placeholder_no_prompt() {
        let meta_store = InMemoryMetaStore::new();
        let sid = SessionId::new();
        meta_store.write_meta(sid, &meta(None, "/repo", 1)).unwrap();
        let store = SessionStore::new(Box::new(InMemoryBackend::new()));
        // No UserInput appended; the log head has no prompt.
        let log: Arc<dyn SessionLog> = Arc::new(store);
        let bridge = SessionListerBridge::new(Arc::new(meta_store), log, std::path::PathBuf::new());
        let mut rows = bridge.list_sessions(&SessionId::new().to_string());
        assert_eq!(rows.len(), 1);
        assert!(
            rows[0].title.starts_with("(session) "),
            "placeholder should carry a short sid suffix, got: {}",
            rows[0].title
        );
        assert!(
            rows[0].title.len() > "(session) ".len(),
            "the suffix must add distinguishing info: {}",
            rows[0].title
        );
        // resolve_detail finds no UserInput in the log head -> placeholder
        // stays (InMemoryBackend's read_log_range returns empty).
        bridge.resolve_detail(&mut rows[0]);
        assert!(
            rows[0].title.starts_with("(session) "),
            "placeholder survives resolve_detail when no prompt exists: {}",
            rows[0].title
        );
    }

    /// A disk FileMetaStore + a real disk backend round-trips through both
    /// phases (the prod path the picker uses): list_sessions opens with a
    /// placeholder + the log mtime, resolve_detail fills the slug from the
    /// log head.
    #[tokio::test]
    async fn test_bridge_with_disk_store() {
        let root = temp_root();
        let backend = houyicoder_memory::LocalFileBackend::new(root.clone());
        let store = SessionStore::new(Box::new(backend));
        let sid = SessionId::new();
        store
            .append(TurnEvent {
                id: EventId::new(),
                session: sid,
                ts: 0,
                prev_hash: None,
                kind: TurnEventKind::UserInput {
                    text: "disk prompt here".into(),
                },
            })
            .await
            .unwrap();
        // The sidecar is written by the composition root; the bridge only
        // reads. Write one manually so the bridge finds the session.
        let fms = FileMetaStore::new(root.clone());
        fms.write_meta(sid, &meta(None, "/repo", 1)).unwrap();
        let log: Arc<dyn SessionLog> = Arc::new(store);
        let bridge = SessionListerBridge::new(
            Arc::new(FileMetaStore::new(root.clone())),
            log,
            root.clone(),
        );
        let mut rows = bridge.list_sessions(&SessionId::new().to_string());
        assert_eq!(rows.len(), 1);
        assert!(
            rows[0].title.starts_with("(session) "),
            "phase 1 returns a placeholder: {}",
            rows[0].title
        );
        bridge.resolve_detail(&mut rows[0]);
        assert_eq!(
            rows[0].title, "disk-prompt-here",
            "phase 2 fills the first-prompt slug"
        );
        std::fs::remove_dir_all(&root).ok();
    }
}
