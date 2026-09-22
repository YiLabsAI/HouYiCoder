//! Adapt durable session descriptors and logs to the TUI SessionCatalog port.

use std::sync::Arc;

use houyicoder_api::session::SessionLog;
use houyicoder_context::session_class::{LOG_FILE, recent_user_sessions};
use houyicoder_context::{SessionDescriptorStore, SessionEvent, SessionId, SessionLogEntry};
use houyicoder_tui::resume_picker::{SessionCatalog, SessionRow};

pub struct DescriptorSessionCatalog {
    descriptor_store: Arc<dyn SessionDescriptorStore>,
    session_log: Arc<dyn SessionLog>,
    sessions_root: std::path::PathBuf,
}

impl DescriptorSessionCatalog {
    /// Build both session readers from the same root.
    pub fn new(session_log: Arc<dyn SessionLog>, sessions_root: std::path::PathBuf) -> Self {
        let descriptor_store =
            houyicoder_service::composition::disk_descriptor_store_at(sessions_root.clone());
        Self {
            descriptor_store,
            session_log,
            sessions_root,
        }
    }
}

impl SessionCatalog for DescriptorSessionCatalog {
    fn sessions(&self, current_sid: &str) -> Vec<SessionRow> {
        let current = SessionId::from_display_string(current_sid).unwrap_or_default();
        // The listing already holds the user's own resumable sessions, ranked
        // by last-active, so this only applies the visible limit.
        const VISIBLE_LIMIT: usize = 100;
        let recent = recent_user_sessions(&self.sessions_root, VISIBLE_LIMIT);
        let mut rows: Vec<SessionRow> = recent
            .into_iter()
            .filter(|entry| entry.sid != current)
            .filter_map(|entry| {
                // A user session always has its descriptor, and the entry carries
                // both it and the directory it was scanned from, so a row costs
                // no second read.
                let descriptor = entry.descriptor?;
                let cwd_basename = descriptor
                    .cwd
                    .rsplit('/')
                    .next()
                    .filter(|s| !s.is_empty())
                    .unwrap_or("?")
                    .to_string();
                let title = descriptor
                    .name
                    .as_ref()
                    .filter(|n| !n.trim().is_empty())
                    .cloned()
                    .unwrap_or_else(|| format!("(session) {}", short_sid(entry.sid)));
                let log_size = std::fs::metadata(entry.path.join(LOG_FILE))
                    .map(|m| m.len())
                    .unwrap_or(0);
                Some(SessionRow {
                    sid_str: entry.sid.to_string(),
                    title,
                    cwd_basename,
                    last_active: entry.last_active,
                    log_size,
                    ..Default::default()
                })
            })
            .collect();
        // Already sorted by last_active desc by the listing, which also already
        // dropped every non-user session; only the current session is removed
        // above, and a removal preserves order -- so no re-sort is needed.
        // Dedup by the cheap title: when multiple sessions share the same
        // descriptor name (the common "re-running + naming alike" case), keep
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
        // Re-read the descriptor to decide the title unambiguously: a user-set
        // name wins (even if it happens to start with "(session)", which the
        // old starts_with heuristic would have mistaken for a placeholder).
        // Only when there is no descriptor name do we pay the log-head read +
        // serde parse for the first-prompt slug. last_active is already the
        // log mtime the listing carried, so no re-stat here.
        let has_name = self
            .descriptor_store
            .read_descriptor(sid)
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
        if let Ok(ev) = serde_json::from_str::<SessionLogEntry>(line)
            && let SessionEvent::UserInput { text } = &ev.event
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
        let mut truncated: String = out.chars().take(39).collect();
        while truncated.ends_with('-') {
            truncated.pop();
        }
        truncated.push('\u{2026}');
        truncated
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use houyicoder_context::{
        EventId, NameSource, SessionDescriptor, SessionEvent, SessionId, SessionLogEntry,
        SessionProvenance,
    };
    use houyicoder_memory::{FileDescriptorStore, LocalFileBackend};
    use houyicoder_session::SessionStore;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_root() -> std::path::PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("session-catalog-{}-{n}", std::process::id()));
        let _r = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn descriptor(name: Option<&str>, cwd: &str, ts: u64) -> SessionDescriptor {
        SessionDescriptor {
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

    /// Write a descriptor for a session at the root (real disk, one truth
    /// source with the catalog's sessions_root).
    fn write_descriptor(root: &std::path::Path, sid: SessionId, m: &SessionDescriptor) {
        let store = FileDescriptorStore::new(root.to_path_buf());
        store.write_descriptor(sid, m).unwrap();
    }

    /// Stamp a path's mtime to N seconds ago so the last-active sort is
    /// deterministic. The listing resolves mtime at whole-second
    /// granularity, so two sessions written in the same second tie and the
    /// sort falls back to readdir order (non-deterministic); ageing each to
    /// a distinct second pins the order the tests assert on. The descriptor's
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
            .append(SessionLogEntry {
                id: EventId::new(),
                session: sid,
                ts: 0,
                prev_hash: None,
                event: SessionEvent::UserInput { text: text.into() },
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_catalog_lists_derives_titles() {
        let root = temp_root();
        let cur = SessionId::new();
        let older = SessionId::new();
        let newer = SessionId::new();
        write_descriptor(&root, older, &descriptor(None, "/repo/a", 1));
        write_descriptor(
            &root,
            newer,
            &descriptor(Some("named session"), "/repo/b", 1),
        );
        write_descriptor(&root, cur, &descriptor(None, "/repo/c", 1));
        let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
        append_log(&store, older, "hello world prompt").await;
        append_log(&store, newer, "named session prompt").await;
        // Pin distinct whole-second log mtimes: older newest (sorts first),
        // newer second. age() is the only ordering signal (created_at above
        // is constant and does not sort).
        age(&root.join(older.to_string()).join("log.jsonl"), 100);
        age(&root.join(newer.to_string()).join("log.jsonl"), 200);
        let log: Arc<dyn SessionLog> = Arc::new(store);
        let catalog = DescriptorSessionCatalog::new(log, root.clone());
        let mut rows = catalog.sessions(&cur.to_string());
        assert_eq!(rows.len(), 2, "current session excluded: {rows:?}");
        assert_eq!(
            rows[0].sid_str,
            older.to_string(),
            "newer log mtime sorts first"
        );
        assert!(
            rows[0].title.starts_with("(session) "),
            "sessions returns a placeholder, not the slug: {}",
            rows[0].title
        );
        assert_eq!(
            rows[1].sid_str,
            newer.to_string(),
            "older log mtime sorts second"
        );
        assert_eq!(
            rows[1].title, "named session",
            "descriptor name is the cheap title (no log read)"
        );
        assert_eq!(rows[1].cwd_basename, "b");
        catalog.resolve_detail(&mut rows[0]);
        assert_eq!(
            rows[0].title, "hello-world-prompt",
            "resolve_detail fills the first-prompt slug"
        );
        catalog.resolve_detail(&mut rows[1]);
        assert_eq!(
            rows[1].title, "named session",
            "resolve_detail leaves a descriptor name untouched"
        );
        let _r = std::fs::remove_dir_all(&root);
    }

    /// A prompt longer than the slug cap produces an ellipsis-terminated slug
    /// no wider than 40 display columns, with no trailing dash before the
    /// ellipsis.
    #[tokio::test]
    async fn test_long_prompt_slug_ellipsis() {
        let root = temp_root();
        let sid = SessionId::new();
        write_descriptor(&root, sid, &descriptor(None, "/repo", 1));
        let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
        append_log(
            &store,
            sid,
            "project context from the nearest agents memory hicoder today",
        )
        .await;
        age(&root.join(sid.to_string()).join("log.jsonl"), 100);
        let log: Arc<dyn SessionLog> = Arc::new(store);
        let catalog = DescriptorSessionCatalog::new(log, root.clone());
        let mut rows = catalog.sessions(&SessionId::new().to_string());
        assert_eq!(rows.len(), 1);
        catalog.resolve_detail(&mut rows[0]);
        let title = &rows[0].title;
        assert!(
            title.ends_with('\u{2026}'),
            "long slug should end with ellipsis: {title}"
        );
        assert!(
            !title.ends_with("-\u{2026}"),
            "no trailing dash before ellipsis: {title}"
        );
        assert!(
            title.chars().count() <= 40,
            "slug within 40 chars: {title} ({})",
            title.chars().count()
        );
        let _r = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn test_catalog_placeholder_no_prompt() {
        let root = temp_root();
        let sid = SessionId::new();
        write_descriptor(&root, sid, &descriptor(None, "/repo", 1));
        let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
        // Append an empty UserInput so the session has a log (resumable +
        // listed) but slugifies to nothing -- the title stays the sid
        // placeholder, the property under test.
        append_log(&store, sid, "").await;
        let log: Arc<dyn SessionLog> = Arc::new(store);
        let catalog = DescriptorSessionCatalog::new(log, root.clone());
        let mut rows = catalog.sessions(&SessionId::new().to_string());
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
        catalog.resolve_detail(&mut rows[0]);
        assert!(
            rows[0].title.starts_with("(session) "),
            "placeholder survives resolve_detail when no prompt exists: {}",
            rows[0].title
        );
        let _r = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn test_catalog_dedup_by_title() {
        let root = temp_root();
        let a = SessionId::new();
        let b = SessionId::new();
        let c = SessionId::new();
        write_descriptor(&root, a, &descriptor(Some("shared"), "/repo", 1));
        write_descriptor(&root, b, &descriptor(Some("shared"), "/repo", 1));
        write_descriptor(&root, c, &descriptor(Some("unique"), "/repo", 1));
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
        let catalog = DescriptorSessionCatalog::new(log, root.clone());
        let rows = catalog.sessions(&SessionId::new().to_string());
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

    /// A directory named in the legacy id spelling is reachable: the sid prints
    /// as a UUID, so a reader that rebuilds the directory from the id finds
    /// nothing and the row disappears. The listing carries the scanned
    /// directory and the descriptor, so the row renders either way.
    #[tokio::test]
    async fn test_catalog_reads_legacy_name() {
        const LEGACY: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
        let root = temp_root();
        let dir = root.join(LEGACY);
        std::fs::create_dir_all(&dir).unwrap();
        let sid = SessionId::from_display_string(LEGACY).unwrap();
        assert_ne!(
            dir,
            root.join(sid.to_string()),
            "the fixture is only meaningful while the two spellings differ"
        );
        std::fs::write(
            dir.join("session.json"),
            serde_json::to_vec(&descriptor(Some("legacy session"), "/repo", 1)).unwrap(),
        )
        .unwrap();
        let entry = SessionLogEntry {
            id: EventId::new(),
            session: sid,
            ts: 0,
            prev_hash: None,
            event: SessionEvent::UserInput {
                text: "legacy prompt".into(),
            },
        };
        std::fs::write(
            dir.join("log.jsonl"),
            format!("{}\n", serde_json::to_string(&entry).unwrap()),
        )
        .unwrap();

        let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
        let log: Arc<dyn SessionLog> = Arc::new(store);
        let catalog = DescriptorSessionCatalog::new(log, root.clone());
        let rows = catalog.sessions(&SessionId::new().to_string());
        assert_eq!(rows.len(), 1, "a legacy-named session is still a row");
        assert_eq!(rows[0].sid_str, sid.to_string());
        assert_eq!(rows[0].title, "legacy session");
        assert!(
            rows[0].log_size > 0,
            "the size is read from the directory the scan found"
        );
        let _r = std::fs::remove_dir_all(&root);
    }
}
