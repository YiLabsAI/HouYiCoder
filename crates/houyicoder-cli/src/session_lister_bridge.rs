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
    /// Construct from one truth source: sessions_root. The meta store is
    /// derived from the same root (a FileMetaStore pointed at it), so
    /// discovery (readdir) and metadata reading (read_meta) can never
    /// disagree about which sessions exist.
    pub fn new(session_log: Arc<dyn SessionLog>, sessions_root: std::path::PathBuf) -> Self {
        let meta_store = houyicoder_service::composition::disk_meta_store_at(sessions_root.clone());
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
        // Stat-first: read_dir + stat log.jsonl mtime for ALL sessions
        // (no JSON parse), sort by last-active, take the top 100. Only
        // those 100 pay the sidecar serde cost (read_meta). On a 50k
        // backlog this replaces 50k JSON parses with 50k stats + 100
        // parses. A session without a log is skipped: resume_sid
        // hard-errors on a missing log, so a no-log row -- even one
        // with a sidecar -- is a row the user cannot resume. Skipping
        // them at the stat phase keeps the visible slots full of rows
        // that are actually actionable.
        const VISIBLE_LIMIT: usize = 100;
        let recent = houyicoder_service::session_prune::list_recent_sessions(
            &self.sessions_root,
            VISIBLE_LIMIT,
        );
        let mut rows: Vec<SessionRow> = recent
            .into_iter()
            .filter(|(sid, _)| *sid != current)
            .filter_map(|(sid, last_active)| {
                let meta = self.meta_store.read_meta(sid)?;
                let cwd_basename = meta
                    .cwd
                    .rsplit('/')
                    .next()
                    .filter(|s| !s.is_empty())
                    .unwrap_or("?")
                    .to_string();
                let title = meta
                    .name
                    .as_ref()
                    .filter(|n| !n.trim().is_empty())
                    .cloned()
                    .unwrap_or_else(|| format!("(session) {}", short_sid(sid)));
                Some(SessionRow {
                    sid_str: sid.to_string(),
                    title,
                    cwd_basename,
                    last_active,
                    ..Default::default()
                })
            })
            .collect();
        // Already sorted by last_active desc from list_recent_sessions,
        // but filter_map may have dropped entries (read_meta None), so
        // the order is preserved -- no re-sort needed.
        // Dedup by the cheap title: when multiple sessions share the same
        // sidecar name (the common "re-running + naming alike" case), keep
        // only the most recently active one. The sort put the newest first,
        // so the first occurrence of each title wins. Placeholder titles are
        // unique (short sid suffix), so unnamed sessions never dedup here --
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
            // whitespace) slugifies to empty -- keep the disambiguating
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
    use houyicoder_memory::{FileMetaStore, LocalFileBackend};
    use houyicoder_session::SessionStore;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_root() -> std::path::PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("lister-bridge-{}-{n}", std::process::id()));
        let _r = std::fs::remove_dir_all(&p);
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
            child_session_ids: Vec::new(),
        }
    }

    /// Write a sidecar for a session at the root (real disk, one truth
    /// source with the bridge's sessions_root).
    fn write_sidecar(root: &std::path::Path, sid: SessionId, m: &SessionMeta) {
        let store = FileMetaStore::new(root.to_path_buf());
        store.write_meta(sid, m).unwrap();
    }

    /// Stamp a path's mtime to N seconds ago so the stat-first sort is
    /// deterministic. list_recent_sessions resolves mtime at whole-second
    /// granularity, so two sessions written in the same second tie and the
    /// sort falls back to readdir order (non-deterministic); ageing each to
    /// a distinct second pins the order the tests assert on. The sidecar's
    /// created_at field does NOT participate in the sort -- only this mtime
    /// does -- so age() is the single ordering signal in these tests.
    ///
    /// Opened for write, not read: on windows the underlying call demands
    /// the write-attributes right, which a read-only handle does not carry.
    /// Every path passed here is a file, so one open serves both platforms.
    fn age(path: &std::path::Path, secs_ago: u64) {
        use std::time::{Duration, SystemTime};
        let t = SystemTime::now() - Duration::from_secs(secs_ago);
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open for set_times");
        f.set_times(std::fs::FileTimes::new().set_modified(t))
            .expect("set mtime");
    }

    /// Append a UserInput to a session's log so it is resumable + listed by
    /// the picker (a session without a log is skipped -- resume_sid
    /// hard-errors on a missing log).
    async fn append_log(store: &SessionStore, sid: SessionId, text: &str) {
        store
            .append(TurnEvent {
                id: EventId::new(),
                session: sid,
                ts: 0,
                prev_hash: None,
                kind: TurnEventKind::UserInput { text: text.into() },
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_bridge_lists_derives_titles() {
        let root = temp_root();
        let cur = SessionId::new();
        let older = SessionId::new();
        let newer = SessionId::new();
        write_sidecar(&root, older, &meta(None, "/repo/a", 1));
        write_sidecar(&root, newer, &meta(Some("named session"), "/repo/b", 1));
        write_sidecar(&root, cur, &meta(None, "/repo/c", 1));
        let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
        append_log(&store, older, "hello world prompt").await;
        append_log(&store, newer, "named session prompt").await;
        // Pin distinct whole-second log mtimes: older newest (sorts first),
        // newer second. age() is the only ordering signal (created_at above
        // is constant and does not sort).
        age(&root.join(older.to_string()).join("log.jsonl"), 100);
        age(&root.join(newer.to_string()).join("log.jsonl"), 200);
        let log: Arc<dyn SessionLog> = Arc::new(store);
        let bridge = SessionListerBridge::new(log, root.clone());
        let mut rows = bridge.list_sessions(&cur.to_string());
        assert_eq!(rows.len(), 2, "current session excluded: {rows:?}");
        assert_eq!(
            rows[0].sid_str,
            older.to_string(),
            "newer log mtime sorts first"
        );
        assert!(
            rows[0].title.starts_with("(session) "),
            "list_sessions returns a placeholder, not the slug: {}",
            rows[0].title
        );
        assert_eq!(
            rows[1].sid_str,
            newer.to_string(),
            "older log mtime sorts second"
        );
        assert_eq!(
            rows[1].title, "named session",
            "sidecar name is the cheap title (no log read)"
        );
        assert_eq!(rows[1].cwd_basename, "b");
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
        let _r = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn test_bridge_placeholder_no_prompt() {
        let root = temp_root();
        let sid = SessionId::new();
        write_sidecar(&root, sid, &meta(None, "/repo", 1));
        let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
        // Append an empty UserInput so the session has a log (resumable +
        // listed) but slugifies to nothing -- the title stays the sid
        // placeholder, the property under test.
        append_log(&store, sid, "").await;
        let log: Arc<dyn SessionLog> = Arc::new(store);
        let bridge = SessionListerBridge::new(log, root.clone());
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
        bridge.resolve_detail(&mut rows[0]);
        assert!(
            rows[0].title.starts_with("(session) "),
            "placeholder survives resolve_detail when no prompt exists: {}",
            rows[0].title
        );
        let _r = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn test_bridge_dedup_by_title() {
        let root = temp_root();
        let a = SessionId::new();
        let b = SessionId::new();
        let c = SessionId::new();
        write_sidecar(&root, a, &meta(Some("shared"), "/repo", 1));
        write_sidecar(&root, b, &meta(Some("shared"), "/repo", 1));
        write_sidecar(&root, c, &meta(Some("unique"), "/repo", 1));
        let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
        append_log(&store, a, "a prompt").await;
        append_log(&store, b, "b prompt").await;
        append_log(&store, c, "c prompt").await;
        // Pin distinct whole-second log mtimes: a newest (wins the "shared"
        // dedup -- first occurrence kept), c middle (survives, unique
        // title), b oldest (dropped). age() is the only ordering signal.
        age(&root.join(a.to_string()).join("log.jsonl"), 100);
        age(&root.join(b.to_string()).join("log.jsonl"), 300);
        age(&root.join(c.to_string()).join("log.jsonl"), 200);
        let log: Arc<dyn SessionLog> = Arc::new(store);
        let bridge = SessionListerBridge::new(log, root.clone());
        let rows = bridge.list_sessions(&SessionId::new().to_string());
        assert_eq!(rows.len(), 2, "dedup drops one of the shared-title pair");
        assert!(
            rows.iter().any(|r| r.sid_str == a.to_string()),
            "newer shared-title session wins the dedup"
        );
        assert!(
            rows.iter().any(|r| r.sid_str == c.to_string()),
            "unique title session survives dedup"
        );
        assert!(
            !rows.iter().any(|r| r.sid_str == b.to_string()),
            "older shared-title session is dropped"
        );
        let _r = std::fs::remove_dir_all(&root);
    }
}
